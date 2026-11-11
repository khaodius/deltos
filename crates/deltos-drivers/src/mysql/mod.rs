//! MySQL and MariaDB, through mysql_async with rustls.
//!
//! Like the PostgreSQL driver, a [`MysqlConnection`] keeps a metadata session and a
//! query session. A statement's rows are read by a task that owns the query session and
//! passes batches through a channel with room for one, waiting for room before it reads
//! further. Running another statement, or dropping the stream, stops that task: it sends
//! `KILL QUERY` from a short-lived side connection, drains what the server already sent
//! and hands the session back.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use deltos_data::{
    BrowsePage, BrowseRequest, CancelHandle, Capabilities, Column, Connection, ConnectionConfig,
    Driver, DriverKind, Error, ExecuteOptions, Namespace, ObjectEntry, ObjectInfo, ObjectRef,
    Result, Row, RowBatch, RowEdit, RowStream, TlsMode, Value,
};
use futures::future::BoxFuture;
use mysql_async::prelude::Queryable;
use mysql_async::{Conn, Opts, OptsBuilder, SslOpts};
use tokio::runtime::Handle;
use tokio::sync::{Notify, OwnedMutexGuard, mpsc, oneshot};

use crate::sql::{self, Dialect};
use crate::{runtime, util};

mod decode;
mod introspect;

use decode::ColumnInfo;

const DIALECT: Dialect = Dialect::Mysql;

/// Opens MySQL and MariaDB connections.
#[derive(Debug, Clone, Copy, Default)]
pub struct MysqlDriver;

impl Driver for MysqlDriver {
    fn kind(&self) -> DriverKind {
        DriverKind::Mysql
    }

    fn connect<'a>(
        &'a self,
        config: &'a ConnectionConfig,
    ) -> BoxFuture<'a, Result<Arc<dyn Connection>>> {
        let config = config.clone();
        Box::pin(async move {
            let rt = runtime::handle()?;
            let connection = runtime::spawn(&rt.clone(), async move {
                MysqlConnection::open(&config, rt).await
            })
            .await?;
            Ok(Arc::new(connection) as Arc<dyn Connection>)
        })
    }
}

/// An open MySQL or MariaDB server connection.
pub struct MysqlConnection {
    inner: Arc<Inner>,
}

struct Inner {
    rt: Handle,
    opts: Opts,
    version: String,
    meta: tokio::sync::Mutex<Option<Conn>>,
    query: Arc<tokio::sync::Mutex<Option<QuerySession>>>,
    /// Stops the task reading the open result, if there is one.
    current: Mutex<Option<Arc<Notify>>>,
    /// The connection id of the statement running on the query session.
    running: Mutex<Option<u32>>,
}

struct QuerySession {
    conn: Conn,
    database: Option<String>,
}

impl MysqlConnection {
    async fn open(config: &ConnectionConfig, rt: Handle) -> Result<Self> {
        let (opts, mode) = mysql_opts(config)?;
        let (mut conn, opts) = connect_with_fallback(opts, mode).await?;
        let banner: Option<String> = conn
            .query_first("SELECT VERSION()")
            .await
            .map_err(my_error)?;
        let banner = banner.unwrap_or_default();
        let number = banner.split('-').next().unwrap_or("").to_string();
        let version = if banner.to_ascii_lowercase().contains("mariadb") {
            format!("MariaDB {number}")
        } else {
            format!("MySQL {number}")
        };
        Ok(Self {
            inner: Arc::new(Inner {
                rt,
                opts,
                version,
                meta: tokio::sync::Mutex::new(Some(conn)),
                query: Arc::new(tokio::sync::Mutex::new(None)),
                current: Mutex::new(None),
                running: Mutex::new(None),
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
    /// Runs `f` on the metadata session, reconnecting first when it was lost.
    async fn with_meta<T>(
        &self,
        f: impl for<'c> FnOnce(&'c mut Conn) -> BoxFuture<'c, Result<T>>,
    ) -> Result<T> {
        let mut slot = self.meta.lock().await;
        if slot.is_none() {
            *slot = Some(Conn::new(self.opts.clone()).await.map_err(connect_error)?);
        }
        let Some(conn) = slot.as_mut() else {
            return Err(Error::Closed);
        };
        let result = f(conn).await;
        if let Err(Error::Closed) = &result {
            *slot = None;
        }
        result
    }

    /// Asks the task reading the open result to stop.
    fn stop_current(&self) {
        if let Some(stop) = self.current.lock().ok().and_then(|mut c| c.take()) {
            stop.notify_one();
        }
    }
}

impl Connection for MysqlConnection {
    fn kind(&self) -> DriverKind {
        DriverKind::Mysql
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
        self.inner.version.clone()
    }

    fn namespaces(&self) -> BoxFuture<'_, Result<Vec<Namespace>>> {
        self.spawn(|inner| async move {
            inner
                .with_meta(|conn| Box::pin(introspect::namespaces(conn)))
                .await
        })
    }

    fn objects<'a>(&'a self, namespace: &'a [String]) -> BoxFuture<'a, Result<Vec<ObjectEntry>>> {
        let namespace = namespace.to_vec();
        self.spawn(|inner| async move {
            let Some(db) = namespace.first().cloned() else {
                return Ok(Vec::new());
            };
            inner
                .with_meta(move |conn| {
                    Box::pin(async move { introspect::objects(conn, &db).await })
                })
                .await
        })
    }

    fn describe<'a>(&'a self, object: &'a ObjectRef) -> BoxFuture<'a, Result<ObjectInfo>> {
        let object = object.clone();
        self.spawn(|inner| async move {
            inner
                .with_meta(move |conn| {
                    Box::pin(async move { introspect::describe(conn, &object).await })
                })
                .await
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
            inner
                .with_meta(move |conn| {
                    Box::pin(async move { browse(conn, &object, &request).await })
                })
                .await
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
            inner
                .with_meta(move |conn| {
                    Box::pin(async move { count(conn, &object, &request, exact).await })
                })
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
            inner
                .with_meta(move |conn| {
                    Box::pin(async move { apply_edits(conn, &object, &edits).await })
                })
                .await
        })
    }

    fn cancel_handle(&self) -> Option<Arc<dyn CancelHandle>> {
        Some(Arc::new(Cancel(self.inner.clone())))
    }

    fn close(&self) -> BoxFuture<'_, ()> {
        self.inner.stop_current();
        Box::pin(async {})
    }
}

/// Kills the statement running on the query session.
struct Cancel(Arc<Inner>);

impl CancelHandle for Cancel {
    fn cancel(&self) -> BoxFuture<'static, Result<()>> {
        let inner = self.0.clone();
        runtime::spawn(&self.0.rt, async move {
            let running = inner.running.lock().ok().and_then(|r| *r);
            match running {
                Some(id) => kill_query(&inner.opts, id).await,
                None => Ok(()),
            }
        })
    }
}

async fn kill_query(opts: &Opts, id: u32) -> Result<()> {
    let mut side = Conn::new(opts.clone()).await.map_err(connect_error)?;
    let result = side
        .query_drop(format!("KILL QUERY {id}"))
        .await
        .map_err(my_error);
    let _ = side.disconnect().await;
    result
}

// ---------------------------------------------------------------------------------------
// Statements.

/// The start of a result: its columns, or what a statement without rows reports.
struct Header {
    columns: Vec<(Column, ColumnInfo)>,
    affected: Option<u64>,
    messages: Vec<String>,
}

async fn execute(inner: Arc<Inner>, sql: String, options: ExecuteOptions) -> Result<RowStream> {
    inner.stop_current();
    let mut guard = inner.query.clone().lock_owned().await;
    // A session lost to the network is dropped where the failure shows, and opened
    // again here.
    if guard.is_none() {
        let conn = Conn::new(inner.opts.clone()).await.map_err(connect_error)?;
        let database = inner.opts.db_name().map(str::to_string);
        *guard = Some(QuerySession { conn, database });
    }
    let Some(session) = guard.as_mut() else {
        return Err(Error::Closed);
    };
    if let Some(db) = options.namespace.as_ref().and_then(|n| n.first())
        && session.database.as_deref() != Some(db.as_str())
    {
        let statement = format!("USE {}", DIALECT.quote_ident(db));
        if let Err(e) = session.conn.query_drop(statement).await {
            let e = my_error(e);
            if matches!(e, Error::Closed) {
                *guard = None;
            }
            return Err(e);
        }
        session.database = Some(db.clone());
    }

    let pieces = sql::split_statements(&sql, DIALECT);
    let Some((last, before)) = pieces.split_last() else {
        return Ok(RowStream::affected(None));
    };
    if let Ok(mut running) = inner.running.lock() {
        *running = Some(session.conn.id());
    }
    for piece in before {
        if let Err(e) = session.conn.query_drop(&sql[piece.clone()]).await {
            clear_running(&inner);
            let e = my_error(e);
            if matches!(e, Error::Closed) {
                *guard = None;
            }
            return Err(e);
        }
    }
    let text = sql[last.clone()].to_string();

    let stop = Arc::new(Notify::new());
    if let Ok(mut current) = inner.current.lock() {
        *current = Some(stop.clone());
    }
    let (header_tx, header_rx) = oneshot::channel();
    let (batches_tx, batches_rx) = mpsc::channel(1);
    let task = Producer {
        inner: inner.clone(),
        batch: util::batch_size(options.batch_size),
        stop,
    };
    inner.rt.spawn(task.run(guard, text, header_tx, batches_tx));
    let header = header_rx.await.map_err(|_| Error::Closed)??;
    if header.columns.is_empty() {
        let mut result = RowStream::affected(header.affected);
        result.messages = header.messages;
        return Ok(result);
    }
    Ok(RowStream {
        columns: header.columns.into_iter().map(|(c, _)| c).collect(),
        batches: util::channel_stream(batches_rx),
        affected: None,
        messages: header.messages,
    })
}

fn clear_running(inner: &Inner) {
    if let Ok(mut running) = inner.running.lock() {
        *running = None;
    }
}

/// Reads one statement's rows on the query session, one batch ahead of the reader.
struct Producer {
    inner: Arc<Inner>,
    batch: usize,
    stop: Arc<Notify>,
}

enum Step {
    Row(Option<mysql_async::Row>),
    Failed(mysql_async::Error),
    Stop,
}

impl Producer {
    async fn run(
        self,
        mut guard: OwnedMutexGuard<Option<QuerySession>>,
        sql: String,
        header: oneshot::Sender<Result<Header>>,
        batches: mpsc::Sender<Result<RowBatch>>,
    ) {
        let Some(session) = guard.as_mut() else {
            let _ = header.send(Err(Error::Closed));
            return;
        };
        let id = session.conn.id();
        let counts_rows = sql::counts_rows(&sql::first_keyword(&sql));
        let mut result = match session.conn.query_iter(sql).await {
            Ok(result) => result,
            Err(e) => {
                clear_running(&self.inner);
                let e = my_error(e);
                if matches!(e, Error::Closed) {
                    *guard = None;
                }
                let _ = header.send(Err(e));
                return;
            }
        };
        let columns: Vec<(Column, ColumnInfo)> = result
            .columns_ref()
            .iter()
            .map(|c| {
                let info = ColumnInfo::from_column(c);
                (
                    Column::new(c.name_str(), info.type_name(), info.kind()),
                    info,
                )
            })
            .collect();
        if columns.is_empty() {
            let affected = result.affected_rows();
            let warnings = result.warnings();
            let drained = result.drop_result().await;
            clear_running(&self.inner);
            if let Err(e) = drained {
                let _ = header.send(Err(my_error(e)));
                return;
            }
            let messages = if warnings > 0 {
                warning_messages(&mut session.conn).await
            } else {
                Vec::new()
            };
            let _ = header.send(Ok(Header {
                columns,
                affected: (affected > 0 || counts_rows).then_some(affected),
                messages,
            }));
            return;
        }
        let infos: Vec<ColumnInfo> = columns.iter().map(|(_, info)| *info).collect();
        if header
            .send(Ok(Header {
                columns,
                affected: None,
                messages: Vec::new(),
            }))
            .is_err()
        {
            self.abandon(id, result).await;
            return;
        }
        loop {
            let permit = tokio::select! {
                biased;
                _ = self.stop.notified() => None,
                permit = batches.reserve() => permit.ok(),
            };
            let Some(permit) = permit else {
                self.abandon(id, result).await;
                return;
            };
            let mut rows: Vec<Row> = Vec::with_capacity(self.batch.min(4096));
            let mut finished = false;
            while rows.len() < self.batch {
                let step = tokio::select! {
                    biased;
                    _ = self.stop.notified() => Step::Stop,
                    _ = batches.closed() => Step::Stop,
                    next = result.next() => match next {
                        Ok(row) => Step::Row(row),
                        Err(e) => Step::Failed(e),
                    },
                };
                match step {
                    Step::Row(Some(row)) => rows.push(decode_row(row, &infos)),
                    Step::Row(None) => {
                        finished = true;
                        break;
                    }
                    Step::Failed(e) => {
                        permit.send(Err(my_error(e)));
                        let _ = result.drop_result().await;
                        clear_running(&self.inner);
                        return;
                    }
                    Step::Stop => {
                        drop(permit);
                        self.abandon(id, result).await;
                        return;
                    }
                }
            }
            if !rows.is_empty() {
                permit.send(Ok(RowBatch { rows }));
            }
            if finished {
                let _ = result.drop_result().await;
                clear_running(&self.inner);
                return;
            }
        }
    }

    /// Stops the server producing rows nobody will read, then drains the rest.
    async fn abandon(
        &self,
        id: u32,
        result: mysql_async::QueryResult<'_, 'static, mysql_async::TextProtocol>,
    ) {
        if let Err(e) = kill_query(&self.inner.opts, id).await {
            tracing::debug!("could not stop an abandoned MySQL result: {e}");
        }
        let _ = result.drop_result().await;
        clear_running(&self.inner);
    }
}

async fn warning_messages(conn: &mut Conn) -> Vec<String> {
    let rows: Vec<mysql_async::Row> = conn.query("SHOW WARNINGS").await.unwrap_or_default();
    rows.into_iter()
        .take(100)
        .map(|row| {
            let level = decode::text(row.as_ref(0)).unwrap_or_default();
            let code = decode::text(row.as_ref(1)).unwrap_or_default();
            let message = decode::text(row.as_ref(2)).unwrap_or_default();
            format!("{level} {code}: {message}")
        })
        .collect()
}

fn decode_row(row: mysql_async::Row, infos: &[ColumnInfo]) -> Row {
    // `unwrap_raw` rather than `unwrap`, which panics on a value already taken out.
    let values = row.unwrap_raw();
    let mut out = Vec::with_capacity(infos.len());
    for (value, info) in values.into_iter().zip(infos) {
        out.push(value.map_or(Value::Null, |value| info.decode(value)));
    }
    out.resize(infos.len(), Value::Null);
    util::row(out)
}

// ---------------------------------------------------------------------------------------
// Browsing.

async fn browse(
    conn: &mut Conn,
    object: &ObjectRef,
    request: &BrowseRequest,
) -> Result<BrowsePage> {
    let info = introspect::TableInfo::load(conn, object).await?;
    let order = sql::order_keys(&request.sort, &info.tie_break, |c| info.nullable(c));
    let mut select_list = String::new();
    for (i, column) in info.columns.iter().enumerate() {
        if i > 0 {
            select_list.push_str(", ");
        }
        DIALECT.push_ident(&mut select_list, &column.name);
    }
    let from = info.table();
    let built = sql::BrowseSql {
        dialect: DIALECT,
        select_list: &select_list,
        from: &from,
        order: &order,
        total: !info.tie_break.is_empty(),
    }
    .build(request)?;

    let limit = request.limit as usize;
    let mut result = conn
        .exec_iter(built.sql.as_str(), decode::params(&built.params))
        .await
        .map_err(my_error)?;
    let infos: Vec<ColumnInfo> = result
        .columns_ref()
        .iter()
        .map(ColumnInfo::from_column)
        .collect();
    let mut rows = Vec::with_capacity(limit.min(10_000));
    let mut more = false;
    while let Some(row) = result.next().await.map_err(my_error)? {
        if rows.len() == limit {
            more = true;
            continue;
        }
        rows.push(decode_row(row, &infos));
    }
    drop(result);

    let columns: Vec<Column> = info.columns.iter().map(|c| c.column()).collect();
    let last_key = match rows.last() {
        Some(last) if !info.tie_break.is_empty() => order
            .iter()
            .map(|key| {
                let i = columns.iter().position(|c| c.name == key.column)?;
                Some((key.column.clone(), last.get(i)?.clone()))
            })
            .collect::<Option<Vec<_>>>(),
        _ => None,
    };
    Ok(BrowsePage {
        columns: columns.into(),
        rows,
        more,
        last_key,
    })
}

async fn count(
    conn: &mut Conn,
    object: &ObjectRef,
    request: &BrowseRequest,
    exact: bool,
) -> Result<u64> {
    let db = introspect::database_of(object)?;
    let from = DIALECT.qualified(&[db], &object.name);
    let (count_sql, params) = sql::count_sql(DIALECT, &from, request)?;
    if !exact {
        let unfiltered = request.filters.is_empty()
            && request
                .raw_where
                .as_deref()
                .is_none_or(|r| r.trim().is_empty());
        if unfiltered {
            if let Some(n) = introspect::table_rows(conn, db, &object.name).await? {
                return Ok(n);
            }
        } else if let Some(n) = explain_rows(conn, &count_sql, &params).await {
            return Ok(n);
        }
    }
    let n: Option<mysql_async::Value> = conn
        .exec_first(count_sql.as_str(), decode::params(&params))
        .await
        .map_err(my_error)?;
    Ok(decode::number(n.as_ref()).unwrap_or(0))
}

/// The optimizer's row estimate for a statement, from `EXPLAIN`.
async fn explain_rows(conn: &mut Conn, statement: &str, params: &[Value]) -> Option<u64> {
    let explain = format!("EXPLAIN {statement}");
    let mut result = conn
        .exec_iter(explain.as_str(), decode::params(params))
        .await
        .ok()?;
    let names: Vec<String> = result
        .columns_ref()
        .iter()
        .map(|c| c.name_str().to_ascii_lowercase())
        .collect();
    let rows_at = names.iter().position(|n| n == "rows")?;
    let filtered_at = names.iter().position(|n| n == "filtered");
    let first = result.next().await.ok()??;
    let _ = result.drop_result().await;
    let rows = decode::number(first.as_ref(rows_at))? as f64;
    let filtered = filtered_at
        .and_then(|i| decode::text(first.as_ref(i)))
        .and_then(|t| t.parse::<f64>().ok())
        .unwrap_or(100.0);
    Some((rows * filtered / 100.0) as u64)
}

async fn apply_edits(conn: &mut Conn, object: &ObjectRef, edits: &[RowEdit]) -> Result<u64> {
    let db = introspect::database_of(object)?;
    let table = DIALECT.qualified(&[db], &object.name);
    conn.query_drop("START TRANSACTION")
        .await
        .map_err(my_error)?;
    let result: Result<u64> = async {
        let mut total = 0u64;
        for edit in edits {
            let Some(statement) = sql::edit_sql(DIALECT, &table, edit)? else {
                continue;
            };
            conn.exec_drop(statement.sql.as_str(), decode::params(&statement.params))
                .await
                .map_err(my_error)?;
            let n = conn.affected_rows();
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
            conn.query_drop("COMMIT").await.map_err(my_error)?;
            Ok(total)
        }
        Err(e) => {
            let _ = conn.query_drop("ROLLBACK").await;
            Err(e)
        }
    }
}

// ---------------------------------------------------------------------------------------
// Configuration and errors.

/// The connection options for `config` and the TLS mode to use.
fn mysql_opts(config: &ConnectionConfig) -> Result<(Opts, TlsMode)> {
    let mut mode = config.tls;
    let mut url_has_tls = false;
    let builder = match config
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
            let opts = Opts::from_url(&url)
                .map_err(|e| Error::Connect(format!("the connection URL is not valid: {e}")))?;
            url_has_tls = opts.ssl_opts().is_some();
            let missing_user = opts.user().is_none();
            let missing_pass = opts.pass().is_none();
            let missing_db = opts.db_name().is_none();
            let mut builder = OptsBuilder::from_opts(opts);
            if missing_user && !config.user.is_empty() {
                builder = builder.user(Some(config.user.clone()));
            }
            if missing_pass {
                builder = builder.pass(config.password.clone());
            }
            if missing_db && !config.database.trim().is_empty() {
                builder = builder.db_name(Some(config.database.trim().to_string()));
            }
            builder
        }
        None => {
            let host = if config.host.trim().is_empty() {
                "localhost"
            } else {
                config.host.trim()
            };
            OptsBuilder::default()
                .ip_or_hostname(host)
                .tcp_port(config.port.unwrap_or(3306))
                .user(Some(if config.user.is_empty() {
                    "root"
                } else {
                    config.user.as_str()
                }))
                .pass(config.password.clone())
                .db_name(
                    (!config.database.trim().is_empty())
                        .then(|| config.database.trim().to_string()),
                )
        }
    };
    let mut builder = builder
        // Edits count the rows a key matched, not only those whose values changed.
        .client_found_rows(true)
        .stmt_cache_size(64)
        .tcp_keepalive(Some(Duration::from_secs(60)))
        .prefer_socket(false);
    let mut init = Vec::new();
    for (key, value) in &config.options {
        match key.as_str() {
            "init" => init = vec![value.clone()],
            // Below, for every session.
            "read_only" => {}
            other => tracing::debug!("ignoring MySQL option {other}"),
        }
    }
    // Every session the connection opens, again after a lost one too, runs these first; a
    // read-only one that the server will not make read-only does not open.
    if util::read_only(config) {
        init.push("SET SESSION TRANSACTION READ ONLY".to_owned());
    }
    if !init.is_empty() {
        builder = builder.init(init);
    }
    if !url_has_tls {
        builder = builder.ssl_opts(ssl_opts(mode));
    }
    Ok((builder.into(), mode))
}

fn ssl_opts(mode: TlsMode) -> Option<SslOpts> {
    match mode {
        TlsMode::Disable => None,
        TlsMode::Prefer | TlsMode::Require => Some(
            SslOpts::default()
                .with_danger_accept_invalid_certs(true)
                .with_danger_skip_domain_validation(true),
        ),
        TlsMode::VerifyFull => Some(SslOpts::default()),
    }
}

/// Connects, and for [`TlsMode::Prefer`] tries again without TLS when the server has
/// none. Returns the options that worked.
async fn connect_with_fallback(opts: Opts, mode: TlsMode) -> Result<(Conn, Opts)> {
    match Conn::new(opts.clone()).await {
        Ok(conn) => Ok((conn, opts)),
        Err(mysql_async::Error::Driver(mysql_async::DriverError::NoClientSslFlagFromServer))
            if mode == TlsMode::Prefer =>
        {
            let plain: Opts = OptsBuilder::from_opts(opts).ssl_opts(None).into();
            let conn = Conn::new(plain.clone()).await.map_err(connect_error)?;
            Ok((conn, plain))
        }
        Err(e) => Err(connect_error(e)),
    }
}

/// Turns `mariadb://` into `mysql://` and reads the `ssl-mode` spellings other clients
/// use, which mysql_async would refuse.
fn clean_url(url: &str) -> (String, Option<TlsMode>) {
    let url = match url.split_once("://") {
        Some((scheme, rest)) if scheme.eq_ignore_ascii_case("mariadb") => format!("mysql://{rest}"),
        _ => url.to_string(),
    };
    let Some((base, query)) = url.split_once('?') else {
        return (url, None);
    };
    let mut mode = None;
    let mut kept = Vec::new();
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        if matches!(
            key.to_ascii_lowercase().as_str(),
            "ssl-mode" | "sslmode" | "ssl_mode"
        ) {
            mode = Some(match value.to_ascii_lowercase().as_str() {
                "disabled" | "disable" => TlsMode::Disable,
                "preferred" | "prefer" => TlsMode::Prefer,
                "required" | "require" => TlsMode::Require,
                _ => TlsMode::VerifyFull,
            });
        } else {
            kept.push(pair);
        }
    }
    let url = if kept.is_empty() {
        base.to_string()
    } else {
        format!("{base}?{}", kept.join("&"))
    };
    (url, mode)
}

/// Server error numbers for a statement stopped by `KILL QUERY` or a time limit.
const INTERRUPTED: [u16; 2] = [1317, 3024];

fn my_error(e: mysql_async::Error) -> Error {
    match e {
        mysql_async::Error::Server(server) if INTERRUPTED.contains(&server.code) => {
            Error::Cancelled
        }
        mysql_async::Error::Server(server) => Error::Query(server.message),
        mysql_async::Error::Driver(mysql_async::DriverError::ConnectionClosed) => Error::Closed,
        mysql_async::Error::Io(e) => {
            tracing::debug!("MySQL connection lost: {e}");
            Error::Closed
        }
        other => Error::Query(other.to_string()),
    }
}

fn connect_error(e: mysql_async::Error) -> Error {
    match e {
        mysql_async::Error::Server(server) => Error::Connect(server.message),
        mysql_async::Error::Driver(mysql_async::DriverError::UnknownAuthPlugin { name }) => {
            Error::Connect(auth_plugin_message(&name))
        }
        other => Error::Connect(other.to_string()),
    }
}

/// What to say when the server wants a sign-in method the driver does not have. Windows
/// installs of MariaDB often default to GSSAPI, and a local one on 3306 is the usual
/// reason to meet it by mistake.
fn auth_plugin_message(plugin: &str) -> String {
    if plugin.contains("gssapi") {
        format!(
            "This server wants Windows/GSSAPI login ({plugin}), which isn't supported. Wrong server or port? Check that you're not hitting a local MariaDB on 3306."
        )
    } else {
        format!(
            "This server wants the {plugin} login method, which isn't supported. Wrong server or port? Check that you're not hitting a local MariaDB or MySQL on 3306."
        )
    }
}

#[cfg(test)]
mod tests;

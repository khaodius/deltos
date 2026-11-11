//! The contract between Deltos and its database drivers.
//!
//! A driver turns a [`ConnectionConfig`] into a [`Connection`]. A connection answers three
//! kinds of question:
//!
//! - **What is there**: namespaces (databases, schemas, keyspaces), the objects inside them
//!   (tables, views, collections) and their structure.
//! - **Browse an object**: random access to any window of rows, with an optional sort and
//!   filter. The table view uses this to jump anywhere in a table of any size without
//!   reading what lies before it. Drivers seek by key where they can and fall back to
//!   offsets where they cannot.
//! - **Run a statement**: a [`RowStream`] of batches that the caller pulls. A driver never
//!   reads ahead of what has been asked for beyond one batch, so memory stays bounded by the
//!   consumer, never by the size of the result.
//!
//! Everything is object safe and boxed, so the app holds `Arc<dyn Connection>` and new
//! database kinds plug in without the UI knowing about them.

use std::fmt;
use std::sync::Arc;

use futures::future::BoxFuture;
use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};

mod value;

pub use value::{Value, ValueKind};

/// Every error a driver returns. Messages are shown to the person as they are, so they
/// should read as sentences and carry the server's own message where there is one.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("could not connect: {0}")]
    Connect(String),
    #[error("{0}")]
    Query(String),
    #[error("{0} is not supported by this database")]
    Unsupported(&'static str),
    #[error("cancelled")]
    Cancelled,
    #[error("the connection was closed")]
    Closed,
    #[error("{0}")]
    Other(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// The kinds of database Deltos knows about. Kinds without a driver yet are listed so
/// saved connections and the UI can name them; [`DriverKind::available`] says which run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DriverKind {
    Postgres,
    Mysql,
    Sqlite,
    Mongodb,
    Cassandra,
    Redis,
    Clickhouse,
    Mssql,
}

impl DriverKind {
    pub const ALL: [DriverKind; 8] = [
        DriverKind::Postgres,
        DriverKind::Mysql,
        DriverKind::Sqlite,
        DriverKind::Mongodb,
        DriverKind::Cassandra,
        DriverKind::Redis,
        DriverKind::Clickhouse,
        DriverKind::Mssql,
    ];

    pub fn label(self) -> &'static str {
        match self {
            DriverKind::Postgres => "PostgreSQL",
            DriverKind::Mysql => "MySQL / MariaDB",
            DriverKind::Sqlite => "SQLite",
            DriverKind::Mongodb => "MongoDB",
            DriverKind::Cassandra => "Cassandra",
            DriverKind::Redis => "Redis",
            DriverKind::Clickhouse => "ClickHouse",
            DriverKind::Mssql => "SQL Server",
        }
    }

    pub fn default_port(self) -> Option<u16> {
        match self {
            DriverKind::Postgres => Some(5432),
            DriverKind::Mysql => Some(3306),
            DriverKind::Sqlite => None,
            DriverKind::Mongodb => Some(27017),
            DriverKind::Cassandra => Some(9042),
            DriverKind::Redis => Some(6379),
            DriverKind::Clickhouse => Some(9000),
            DriverKind::Mssql => Some(1433),
        }
    }

    /// Whether the kind stores documents rather than rows of fixed columns.
    pub fn is_document(self) -> bool {
        matches!(self, DriverKind::Mongodb)
    }
}

impl fmt::Display for DriverKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TlsMode {
    /// Plain connection.
    Disable,
    /// TLS when the server offers it.
    #[default]
    Prefer,
    /// TLS, and fail without it.
    Require,
    /// TLS with the server's certificate checked against the system roots.
    VerifyFull,
}

/// Where and how to connect. The password is not kept here when saved; the app stores it
/// in the system keychain and fills it in before connecting.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ConnectionConfig {
    pub host: String,
    pub port: Option<u16>,
    pub user: String,
    #[serde(skip)]
    pub password: Option<String>,
    /// The database to open first. For SQLite, the file path.
    pub database: String,
    pub tls: TlsMode,
    /// A full connection URL. When set it wins over the fields above, which then only
    /// fill in what the URL leaves out.
    pub url: Option<String>,
    /// Driver specific options, passed through as given.
    pub options: Vec<(String, String)>,
    /// An SSH tunnel to reach the server through. When it is on, `host` and `port` are the
    /// address as seen from the SSH server.
    pub ssh: SshTunnel,
}

/// How to sign in to an SSH server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SshAuth {
    /// A private key file, or the `IdentityFile` entries of `~/.ssh/config` and the usual
    /// key files when no file is given.
    #[default]
    Key,
    /// The keys of a running ssh-agent (or Pageant on Windows).
    Agent,
    Password,
}

/// An SSH tunnel in front of a database connection. The password or key passphrase is not
/// kept here when saved; it goes to the keychain like the database password.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SshTunnel {
    pub enabled: bool,
    /// A host name, an address, or a `Host` alias from `~/.ssh/config`.
    pub host: String,
    /// `None` is the alias's `Port`, or 22.
    pub port: Option<u16>,
    /// Empty is the alias's `User`, or the name signed in to this computer.
    pub user: String,
    pub auth: SshAuth,
    /// The private key file for [`SshAuth::Key`]. Empty uses the alias's `IdentityFile`.
    pub key_path: String,
    /// The SSH password, or the key's passphrase.
    #[serde(skip)]
    pub secret: Option<String>,
}

/// What a connection can do, so the UI offers only what works.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Capabilities {
    pub schemas: bool,
    pub transactions: bool,
    /// Rows can be edited in place through [`Connection::apply_edits`].
    pub edit_rows: bool,
    /// Statements can be cancelled while they run.
    pub cancel: bool,
    /// The query language, for the editor: `sql`, `mongo` and so on.
    pub language: &'static str,
}

/// A database, schema or keyspace. `path` locates it, outermost first: for Postgres
/// `["mydb", "public"]`, for MySQL `["mydb"]`, for SQLite `["main"]`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Namespace {
    pub path: Vec<String>,
    pub kind: NamespaceKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum NamespaceKind {
    Database,
    Schema,
    Keyspace,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ObjectKind {
    Table,
    View,
    MaterializedView,
    Collection,
    Function,
    Procedure,
    Sequence,
    Index,
    Trigger,
}

impl ObjectKind {
    /// Whether rows can be browsed.
    pub fn has_rows(self) -> bool {
        matches!(
            self,
            ObjectKind::Table
                | ObjectKind::View
                | ObjectKind::MaterializedView
                | ObjectKind::Collection
        )
    }
}

/// A table, view, collection or other object, by namespace path and name.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ObjectRef {
    pub namespace: Vec<String>,
    pub name: String,
    pub kind: ObjectKind,
}

/// One entry of a listing, with cheap facts the sidebar can show without another query.
#[derive(Debug, Clone, PartialEq)]
pub struct ObjectEntry {
    pub object: ObjectRef,
    /// The server's estimate of the row count, when it keeps one.
    pub estimated_rows: Option<u64>,
    pub size_bytes: Option<u64>,
    pub comment: Option<String>,
}

/// A result or table column.
#[derive(Debug, Clone, PartialEq)]
pub struct Column {
    pub name: String,
    /// The type as the database names it (`int4`, `varchar(255)`, `objectId`).
    pub type_name: String,
    pub kind: ValueKind,
    pub nullable: Option<bool>,
    /// Position in the primary key, from 0, when it is part of one.
    pub primary_key: Option<u16>,
    pub default: Option<String>,
    pub comment: Option<String>,
}

impl Column {
    pub fn new(name: impl Into<String>, type_name: impl Into<String>, kind: ValueKind) -> Self {
        Self {
            name: name.into(),
            type_name: type_name.into(),
            kind,
            nullable: None,
            primary_key: None,
            default: None,
            comment: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Index {
    pub name: String,
    pub columns: Vec<String>,
    pub unique: bool,
    pub primary: bool,
    pub method: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ForeignKey {
    pub name: String,
    pub columns: Vec<String>,
    pub target: ObjectRef,
    pub target_columns: Vec<String>,
}

/// The structure of one object, for the structure tab and the right-hand info panel.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ObjectInfo {
    pub columns: Vec<Column>,
    pub indexes: Vec<Index>,
    pub foreign_keys: Vec<ForeignKey>,
    pub estimated_rows: Option<u64>,
    pub size_bytes: Option<u64>,
    pub comment: Option<String>,
    /// The statement that would create it, when the database can produce one.
    pub definition: Option<String>,
}

impl ObjectInfo {
    /// The primary key columns in key order.
    pub fn primary_key(&self) -> Vec<&Column> {
        let mut key: Vec<&Column> = self
            .columns
            .iter()
            .filter(|c| c.primary_key.is_some())
            .collect();
        key.sort_by_key(|c| c.primary_key);
        key
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SortDirection {
    Ascending,
    Descending,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sort {
    pub column: String,
    pub direction: SortDirection,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FilterOp {
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    Contains,
    StartsWith,
    IsNull,
    IsNotNull,
}

/// One condition of the table view's filter bar. Conditions are joined with AND.
#[derive(Debug, Clone, PartialEq)]
pub struct Filter {
    pub column: String,
    pub op: FilterOp,
    pub value: Value,
}

/// Which rows of an object to read: `limit` rows starting at row `offset` of the object
/// as sorted and filtered. `raw_where` is a condition in the database's own language,
/// typed by the person, and is used as given.
///
/// Rows come back in a total order: the `sort` columns, then the primary key (or the
/// database's row identity, such as `ctid`, `rowid` or `_id`) as a tie break, so pages
/// never overlap or skip rows.
///
/// `seek_after` is a hint for reading the page that follows one already read: the values,
/// by column name, of the order columns (sort columns, then the tie break key) of the row
/// just before `offset`. A driver that can should then seek by key (`WHERE (a, b) > (..)`)
/// instead of skipping `offset` rows, which keeps scrolling through a table of any size
/// as fast at row 50,000,000 as at row 0. A driver that cannot ignores it and uses
/// `offset`. [`BrowsePage::last_key`] gives the values for the next request.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct BrowseRequest {
    pub offset: u64,
    pub limit: u32,
    pub sort: Vec<Sort>,
    pub filters: Vec<Filter>,
    pub raw_where: Option<String>,
    pub seek_after: Option<Vec<(String, Value)>>,
}

/// A page of rows from [`Connection::browse`].
#[derive(Debug, Clone, PartialEq)]
pub struct BrowsePage {
    pub columns: Arc<[Column]>,
    pub rows: Vec<Row>,
    /// Whether rows exist past this page.
    pub more: bool,
    /// The order column values of the last row, for [`BrowseRequest::seek_after`] of the
    /// next page. `None` when the driver cannot seek.
    pub last_key: Option<Vec<(String, Value)>>,
}

/// One row: a value per column, in column order. Documents are one row per document with
/// the union of the fields seen so far as columns, and nested values as [`Value::Json`].
pub type Row = Box<[Value]>;

/// Rows as they arrive from the database.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RowBatch {
    pub rows: Vec<Row>,
}

/// The result of one statement. `batches` yields rows on demand. A statement that returns
/// no rows (an UPDATE, a CREATE) has no columns, an empty stream and `affected` set when
/// the database reports it.
pub struct RowStream {
    pub columns: Arc<[Column]>,
    pub batches: BoxStream<'static, Result<RowBatch>>,
    pub affected: Option<u64>,
    /// Notices and warnings the server sent with the result.
    pub messages: Vec<String>,
}

impl fmt::Debug for RowStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RowStream")
            .field("columns", &self.columns.len())
            .field("affected", &self.affected)
            .finish_non_exhaustive()
    }
}

impl RowStream {
    /// A result with no rows, for statements that only change things.
    pub fn affected(n: Option<u64>) -> Self {
        Self {
            columns: Arc::from(Vec::new()),
            batches: Box::pin(futures::stream::empty()),
            affected: n,
            messages: Vec::new(),
        }
    }
}

/// Options for [`Connection::execute`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecuteOptions {
    /// Rows per batch. Drivers may round it to what the protocol does well.
    pub batch_size: u32,
    /// The namespace to run in, when the statement does not name one.
    pub namespace: Option<Vec<String>>,
}

impl Default for ExecuteOptions {
    fn default() -> Self {
        Self {
            batch_size: 1000,
            namespace: None,
        }
    }
}

/// A change to one row from the table view: the key identifies it, `changes` lists the
/// new values by column. An insert has an empty key; a delete has no changes.
#[derive(Debug, Clone, PartialEq)]
pub enum RowEdit {
    Update {
        key: Vec<(String, Value)>,
        changes: Vec<(String, Value)>,
    },
    Insert {
        values: Vec<(String, Value)>,
    },
    Delete {
        key: Vec<(String, Value)>,
    },
}

/// Cancels the statement running on a connection, from any thread.
pub trait CancelHandle: Send + Sync {
    fn cancel(&self) -> BoxFuture<'static, Result<()>>;
}

/// An open connection. Methods may be called from several tasks at once; drivers that
/// can run only one statement at a time queue them.
pub trait Connection: Send + Sync {
    fn kind(&self) -> DriverKind;

    fn capabilities(&self) -> Capabilities;

    /// The server's name and version, as shown in the status bar.
    fn server_version(&self) -> String;

    /// Namespaces, outermost first. For Postgres, every database and the schemas of the
    /// current one.
    fn namespaces(&self) -> BoxFuture<'_, Result<Vec<Namespace>>>;

    /// Objects in one namespace.
    fn objects<'a>(&'a self, namespace: &'a [String]) -> BoxFuture<'a, Result<Vec<ObjectEntry>>>;

    /// Columns, indexes and keys of one object.
    fn describe<'a>(&'a self, object: &'a ObjectRef) -> BoxFuture<'a, Result<ObjectInfo>>;

    /// A window of an object's rows. See [`BrowseRequest`].
    fn browse<'a>(
        &'a self,
        object: &'a ObjectRef,
        request: &'a BrowseRequest,
    ) -> BoxFuture<'a, Result<BrowsePage>>;

    /// How many rows match `request`'s filters. `exact` false allows the server's
    /// estimate, which is instant on most databases.
    fn count<'a>(
        &'a self,
        object: &'a ObjectRef,
        request: &'a BrowseRequest,
        exact: bool,
    ) -> BoxFuture<'a, Result<u64>>;

    /// Runs one statement and streams its result.
    fn execute<'a>(
        &'a self,
        statement: &'a str,
        options: &'a ExecuteOptions,
    ) -> BoxFuture<'a, Result<RowStream>>;

    /// Applies edits from the table view in one transaction where the database has them.
    fn apply_edits<'a>(
        &'a self,
        object: &'a ObjectRef,
        edits: &'a [RowEdit],
    ) -> BoxFuture<'a, Result<u64>> {
        let _ = (object, edits);
        Box::pin(async { Err(Error::Unsupported("editing rows")) })
    }

    /// A handle that cancels what this connection is running.
    fn cancel_handle(&self) -> Option<Arc<dyn CancelHandle>> {
        None
    }

    fn close(&self) -> BoxFuture<'_, ()> {
        Box::pin(async {})
    }
}

/// Opens connections of one kind.
pub trait Driver: Send + Sync {
    fn kind(&self) -> DriverKind;

    fn connect<'a>(
        &'a self,
        config: &'a ConnectionConfig,
    ) -> BoxFuture<'a, Result<Arc<dyn Connection>>>;
}

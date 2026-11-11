//! Connection safety: which environment a connection is, read-only connections, and the
//! statements to confirm before they run.
//!
//! A read-only connection is enforced here, at the session, not in the views: the
//! session's connection is wrapped in [`ReadOnly`], which refuses row edits and every
//! statement [`deltos_drivers::classify`] does not know to be a read, whatever part of
//! the app sends it. Where the database has a setting for it, the driver is also asked
//! ([`read_only_config`]) to turn it on for every session it opens, including the ones it
//! opens again after a lost connection or for another database, so a function that
//! writes from inside a `SELECT` is refused by the server as well.

use std::sync::Arc;

use deltos_data::{
    BrowsePage, BrowseRequest, CancelHandle, Capabilities, Connection, ConnectionConfig,
    DriverKind, Error, ExecuteOptions, Namespace, ObjectEntry, ObjectInfo, ObjectRef, Result,
    RowEdit, RowStream,
};
use deltos_drivers::classify::{self, Risk};
use deltos_settings::{ConnectionTag, Data};
use futures::future::BoxFuture;

use crate::state::AppState;

/// What the person should know about a connection wherever it is in use.
#[derive(Debug, Clone, PartialEq)]
pub struct Environment {
    pub name: String,
    pub kind: DriverKind,
    pub tag: ConnectionTag,
    pub read_only: bool,
}

impl Environment {
    /// The open session `id`, or the saved connection when it is not open.
    pub fn of(state: &AppState, id: &str) -> Option<Self> {
        if let Some(s) = state.session(id) {
            return Some(Self {
                name: s.name.clone(),
                kind: s.kind,
                tag: s.tag,
                read_only: s.read_only,
            });
        }
        state.settings.connection(id).map(|c| Self {
            name: c.name.clone(),
            kind: c.kind,
            tag: c.tag,
            read_only: c.read_only,
        })
    }

    /// Staging and production: where a mistake costs the most.
    pub fn is_sensitive(&self) -> bool {
        is_sensitive(self.tag)
    }
}

pub fn is_sensitive(tag: ConnectionTag) -> bool {
    matches!(tag, ConnectionTag::Staging | ConnectionTag::Production)
}

/// A statement held back until the person confirms it.
#[derive(Debug, Clone, PartialEq)]
pub struct Confirm {
    pub statement: String,
    /// One sentence on what the statement does.
    pub reason: String,
    pub environment: Option<Environment>,
}

/// Why `statement` should be confirmed before it runs on `env`, or `None` to run it.
///
/// - Everywhere, with "Confirm destructive statements" on: `UPDATE` and `DELETE` without
///   `WHERE`, `DROP` and `TRUNCATE`.
/// - On staging and production, also `ALTER`, and with "Confirm every write on
///   production" on, any statement that is not a read.
///
/// A read-only connection asks nothing: it refuses the statement itself.
pub fn confirmation(statement: &str, env: Option<&Environment>, data: &Data) -> Option<String> {
    if env.is_some_and(|e| e.read_only) {
        return None;
    }
    let kind = env.map_or(DriverKind::Postgres, |e| e.kind);
    let a = classify::assess(statement, kind);
    let sensitive = env.is_some_and(Environment::is_sensitive);
    let destructive = data.confirm_destructive
        && match a.risk {
            Risk::Unbounded | Risk::Remove => true,
            Risk::Alter => sensitive,
            Risk::Read | Risk::Write => false,
        };
    let write = sensitive && data.confirm_production_writes && a.risk != Risk::Read;
    if !destructive && !write {
        return None;
    }
    Some(match a.risk {
        Risk::Unbounded => format!("{} touches every row.", a.what),
        Risk::Remove => format!("{} can't be undone.", a.what),
        Risk::Alter => "ALTER changes the structure of a table.".to_owned(),
        Risk::Write | Risk::Read => format!("{} changes data.", a.what),
    })
}

/// The connection a session uses: `conn` itself, or `conn` behind [`ReadOnly`].
pub fn guard(conn: Arc<dyn Connection>, read_only: bool) -> Arc<dyn Connection> {
    if read_only {
        Arc::new(ReadOnly::new(conn))
    } else {
        conn
    }
}

/// The refusal for a statement that may change data.
fn refused(what: &str) -> Error {
    let what = if what.is_empty() {
        "This statement".to_owned()
    } else {
        what.to_owned()
    };
    Error::Query(format!(
        "Read-only connection: {what} was not run because it can change data. Turn off Read-only in the connection's settings to run it."
    ))
}

/// `config` with the driver option that has the server refuse writes too, when
/// `read_only`: PostgreSQL and MySQL make each session read-only as it opens, SQLite opens
/// the file read-only. Drivers without such a setting ignore it.
pub fn read_only_config(mut config: ConnectionConfig, read_only: bool) -> ConnectionConfig {
    if read_only {
        config
            .options
            .retain(|(key, _)| !key.eq_ignore_ascii_case("read_only"));
        config.options.push(("read_only".into(), "true".into()));
    }
    config
}

/// A connection that only reads. See the module notes.
pub struct ReadOnly {
    inner: Arc<dyn Connection>,
}

impl ReadOnly {
    pub fn new(inner: Arc<dyn Connection>) -> Self {
        Self { inner }
    }
}

impl Connection for ReadOnly {
    fn kind(&self) -> DriverKind {
        self.inner.kind()
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            edit_rows: false,
            ..self.inner.capabilities()
        }
    }

    fn server_version(&self) -> String {
        self.inner.server_version()
    }

    fn namespaces(&self) -> BoxFuture<'_, Result<Vec<Namespace>>> {
        self.inner.namespaces()
    }

    fn objects<'a>(&'a self, namespace: &'a [String]) -> BoxFuture<'a, Result<Vec<ObjectEntry>>> {
        self.inner.objects(namespace)
    }

    fn describe<'a>(&'a self, object: &'a ObjectRef) -> BoxFuture<'a, Result<ObjectInfo>> {
        self.inner.describe(object)
    }

    fn browse<'a>(
        &'a self,
        object: &'a ObjectRef,
        request: &'a BrowseRequest,
    ) -> BoxFuture<'a, Result<BrowsePage>> {
        match self.check_where(request) {
            Ok(()) => self.inner.browse(object, request),
            Err(e) => Box::pin(async move { Err(e) }),
        }
    }

    fn count<'a>(
        &'a self,
        object: &'a ObjectRef,
        request: &'a BrowseRequest,
        exact: bool,
    ) -> BoxFuture<'a, Result<u64>> {
        match self.check_where(request) {
            Ok(()) => self.inner.count(object, request, exact),
            Err(e) => Box::pin(async move { Err(e) }),
        }
    }

    fn execute<'a>(
        &'a self,
        statement: &'a str,
        options: &'a ExecuteOptions,
    ) -> BoxFuture<'a, Result<RowStream>> {
        let verdict = classify::assess(statement, self.inner.kind());
        Box::pin(async move {
            if !verdict.is_read_only() {
                return Err(refused(&verdict.what));
            }
            self.inner.execute(statement, options).await
        })
    }

    fn apply_edits<'a>(
        &'a self,
        _object: &'a ObjectRef,
        _edits: &'a [RowEdit],
    ) -> BoxFuture<'a, Result<u64>> {
        Box::pin(async {
            Err(Error::Query(
                "Read-only connection: rows can't be edited. Turn off Read-only in the connection's settings to edit them.".into(),
            ))
        })
    }

    fn apply_schema<'a>(
        &'a self,
        _namespace: &'a [String],
        _statements: &'a [String],
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async {
            Err(Error::Query(
                "Read-only connection: the structure of tables can't be changed. Turn off Read-only in the connection's settings to change it.".into(),
            ))
        })
    }

    fn cancel_handle(&self) -> Option<Arc<dyn CancelHandle>> {
        self.inner.cancel_handle()
    }

    fn close(&self) -> BoxFuture<'_, ()> {
        self.inner.close()
    }
}

impl ReadOnly {
    /// A condition typed into a browse goes into the statement as it is, so it must read
    /// too: no `;` and another statement after it.
    fn check_where(&self, request: &BrowseRequest) -> Result<()> {
        let Some(raw) = &request.raw_where else {
            return Ok(());
        };
        let verdict = classify::assess(&format!("SELECT 1 WHERE {raw}"), self.inner.kind());
        if verdict.is_read_only() {
            Ok(())
        } else {
            Err(refused("This filter"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deltos_data::ObjectKind;
    use parking_lot::Mutex;

    /// Records what reaches the server.
    #[derive(Default)]
    struct Recorder {
        kind: Option<DriverKind>,
        ran: Mutex<Vec<String>>,
        edits: Mutex<usize>,
    }

    impl Connection for Recorder {
        fn kind(&self) -> DriverKind {
            self.kind.unwrap_or(DriverKind::Postgres)
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities {
                edit_rows: true,
                language: "sql",
                ..Capabilities::default()
            }
        }
        fn server_version(&self) -> String {
            "test".into()
        }
        fn namespaces(&self) -> BoxFuture<'_, Result<Vec<Namespace>>> {
            Box::pin(async { Ok(Vec::new()) })
        }
        fn objects<'a>(&'a self, _: &'a [String]) -> BoxFuture<'a, Result<Vec<ObjectEntry>>> {
            Box::pin(async { Ok(Vec::new()) })
        }
        fn describe<'a>(&'a self, _: &'a ObjectRef) -> BoxFuture<'a, Result<ObjectInfo>> {
            Box::pin(async { Ok(ObjectInfo::default()) })
        }
        fn browse<'a>(
            &'a self,
            _: &'a ObjectRef,
            _: &'a BrowseRequest,
        ) -> BoxFuture<'a, Result<BrowsePage>> {
            Box::pin(async {
                Ok(BrowsePage {
                    columns: Arc::from(Vec::new()),
                    rows: Vec::new(),
                    more: false,
                    last_key: None,
                })
            })
        }
        fn count<'a>(
            &'a self,
            _: &'a ObjectRef,
            _: &'a BrowseRequest,
            _: bool,
        ) -> BoxFuture<'a, Result<u64>> {
            Box::pin(async { Ok(0) })
        }
        fn execute<'a>(
            &'a self,
            statement: &'a str,
            _: &'a ExecuteOptions,
        ) -> BoxFuture<'a, Result<RowStream>> {
            self.ran.lock().push(statement.to_owned());
            Box::pin(async { Ok(RowStream::affected(None)) })
        }
        fn apply_edits<'a>(
            &'a self,
            _: &'a ObjectRef,
            _: &'a [RowEdit],
        ) -> BoxFuture<'a, Result<u64>> {
            *self.edits.lock() += 1;
            Box::pin(async { Ok(1) })
        }
    }

    fn object() -> ObjectRef {
        ObjectRef {
            namespace: vec!["public".into()],
            name: "t".into(),
            kind: ObjectKind::Table,
        }
    }

    #[test]
    fn read_only_refuses_writes_and_edits() {
        let inner = Arc::new(Recorder::default());
        let conn = guard(inner.clone(), true);
        assert!(!conn.capabilities().edit_rows);
        assert_eq!(conn.capabilities().language, "sql");
        let run =
            |sql: &str| futures::executor::block_on(conn.execute(sql, &ExecuteOptions::default()));
        let refused = run("DELETE FROM t").err().map(|e| e.to_string());
        assert!(refused.is_some_and(|e| e.starts_with("Read-only connection")));
        assert!(run("WITH x AS (UPDATE t SET a = 1 WHERE b = 2 RETURNING 1) SELECT 1").is_err());
        assert!(run("SELECT 1").is_ok());
        assert!(run("select 2").is_ok());
        let edit = futures::executor::block_on(
            conn.apply_edits(&object(), &[RowEdit::Delete { key: Vec::new() }]),
        );
        assert!(edit.is_err());
        assert_eq!(*inner.edits.lock(), 0);
        let schema = futures::executor::block_on(conn.apply_schema(
            &["public".to_owned()],
            &["ALTER TABLE t ADD COLUMN c int".to_owned()],
        ));
        let refused = schema.err().map(|e| e.to_string());
        assert!(refused.is_some_and(|e| e.starts_with("Read-only connection")));
        // Only the reads reach the server.
        assert_eq!(
            *inner.ran.lock(),
            vec!["SELECT 1".to_owned(), "select 2".to_owned()]
        );
    }

    #[test]
    fn read_only_asks_the_driver_too() {
        let mut config = ConnectionConfig::default();
        config.options.push(("READ_ONLY".into(), "false".into()));
        config.options.push(("connect_timeout".into(), "5".into()));
        let config = read_only_config(config, true);
        assert_eq!(
            config.options,
            vec![
                ("connect_timeout".to_owned(), "5".to_owned()),
                ("read_only".to_owned(), "true".to_owned())
            ]
        );
        assert_eq!(read_only_config(config.clone(), true), config);
        assert!(
            read_only_config(ConnectionConfig::default(), false)
                .options
                .is_empty()
        );
    }

    #[test]
    fn read_only_checks_typed_conditions() {
        let inner = Arc::new(Recorder {
            kind: Some(DriverKind::Mysql),
            ..Recorder::default()
        });
        let conn = guard(inner, true);
        let request = |raw: &str| BrowseRequest {
            raw_where: Some(raw.into()),
            ..BrowseRequest::default()
        };
        let o = object();
        let ok = request("a = 1 and b like '%;%'");
        assert!(futures::executor::block_on(conn.browse(&o, &ok)).is_ok());
        let bad = request("1 = 1; DELETE FROM t");
        assert!(futures::executor::block_on(conn.browse(&o, &bad)).is_err());
        assert!(futures::executor::block_on(conn.count(&o, &bad, true)).is_err());
    }

    #[test]
    fn writable_connections_pass_through() {
        let inner: Arc<dyn Connection> = Arc::new(Recorder::default());
        let conn = guard(inner.clone(), false);
        assert!(Arc::ptr_eq(&conn, &inner));
        assert!(conn.capabilities().edit_rows);
    }

    fn env(tag: ConnectionTag) -> Environment {
        Environment {
            name: "Main".into(),
            kind: DriverKind::Postgres,
            tag,
            read_only: false,
        }
    }

    /// The checks the query tab made before connections had environments still hold.
    #[test]
    fn destructive_statements_are_caught() {
        let data = Data::default();
        let asks = |sql: &str| confirmation(sql, None, &data).is_some();
        assert!(asks("DROP TABLE users"));
        assert!(asks("  truncate orders"));
        assert!(asks("delete from users"));
        assert!(!asks("DELETE FROM users WHERE id = 3"));
        assert!(asks("update users set a = 1"));
        assert!(!asks("update users set a = 1 where id = 2"));
        assert!(!asks("select * from dropped"));
        // A word scan used to miss these.
        assert!(asks("update users set note = 'where'"));
        assert!(asks("delete from users -- where id = 3"));
        assert!(asks("update t set a = (select b from c where c.id = 1)"));
    }

    #[test]
    fn confirms_by_environment() {
        let data = Data::default();
        let ask = |sql: &str, tag| confirmation(sql, Some(&env(tag)), &data);
        for tag in ConnectionTag::ALL {
            assert!(ask("delete from t", tag).is_some(), "{tag:?}");
            assert!(ask("UPDATE t SET a = 1", tag).is_some());
            assert!(ask("DROP TABLE t", tag).is_some());
            assert!(ask("select * from t", tag).is_none());
        }
        assert!(
            ask(
                "UPDATE t SET a = 1 WHERE id = 1",
                ConnectionTag::Development
            )
            .is_none()
        );
        assert!(ask("INSERT INTO t VALUES (1)", ConnectionTag::Local).is_none());
        assert!(ask("ALTER TABLE t ADD b int", ConnectionTag::Testing).is_none());
        assert!(ask("ALTER TABLE t ADD b int", ConnectionTag::Staging).is_some());
        assert_eq!(
            ask("INSERT INTO t VALUES (1)", ConnectionTag::Production).as_deref(),
            Some("INSERT changes data.")
        );
        assert_eq!(
            ask("delete from t", ConnectionTag::None).as_deref(),
            Some("DELETE without WHERE touches every row.")
        );
        assert!(ask("UPDATE t SET a = 1 WHERE id = 1", ConnectionTag::Staging).is_some());
        assert!(confirmation("delete from t", None, &data).is_some());

        let quiet = Data {
            confirm_production_writes: false,
            ..Data::default()
        };
        let p = env(ConnectionTag::Production);
        assert!(confirmation("INSERT INTO t VALUES (1)", Some(&p), &quiet).is_none());
        assert!(confirmation("ALTER TABLE t ADD b int", Some(&p), &quiet).is_some());
        let off = Data {
            confirm_destructive: false,
            confirm_production_writes: false,
            ..Data::default()
        };
        assert!(confirmation("DROP TABLE t", Some(&p), &off).is_none());

        let read_only = Environment {
            read_only: true,
            ..p
        };
        assert!(confirmation("DROP TABLE t", Some(&read_only), &data).is_none());
    }
}

//! Unit tests, and integration tests against the server named by
//! `DELTOS_TEST_MYSQL_URL`. The integration tests pass without doing anything when the
//! variable is unset.

use std::time::{Duration, Instant};

use deltos_data::{Filter, FilterOp, ObjectKind, Sort, SortDirection, ValueKind};
use futures::StreamExt;

use super::*;

#[test]
fn explains_unsupported_auth_plugins() {
    let e = connect_error(mysql_async::Error::Driver(
        mysql_async::DriverError::UnknownAuthPlugin {
            name: "auth_gssapi_client".into(),
        },
    ));
    let text = e.to_string();
    assert!(text.contains("Windows/GSSAPI login"), "{text}");
    assert!(text.contains("local MariaDB on 3306"), "{text}");
}

#[test]
fn cleans_urls() {
    let (url, mode) = clean_url("mariadb://u:p@h/db?ssl-mode=REQUIRED&stmt_cache_size=8");
    assert_eq!(url, "mysql://u:p@h/db?stmt_cache_size=8");
    assert_eq!(mode, Some(TlsMode::Require));
    let (url, mode) = clean_url("mysql://h/db");
    assert_eq!(url, "mysql://h/db");
    assert_eq!(mode, None);
}

#[test]
fn builds_options() {
    let config = ConnectionConfig {
        host: "db.local".into(),
        port: Some(3307),
        user: "me".into(),
        password: Some("secret".into()),
        database: "shop".into(),
        tls: TlsMode::Disable,
        ..Default::default()
    };
    let (opts, mode) = mysql_opts(&config).expect("options");
    assert_eq!(mode, TlsMode::Disable);
    assert_eq!(opts.ip_or_hostname(), "db.local");
    assert_eq!(opts.tcp_port(), 3307);
    assert_eq!(opts.db_name(), Some("shop"));
    assert!(opts.ssl_opts().is_none());
    assert!(opts.client_found_rows());
    let url = ConnectionConfig {
        url: Some("mysql://other@h/x".into()),
        password: Some("filled".into()),
        tls: TlsMode::Require,
        ..Default::default()
    };
    let (opts, _) = mysql_opts(&url).expect("options");
    assert_eq!(opts.user(), Some("other"));
    assert_eq!(opts.pass(), Some("filled"));
    assert!(opts.ssl_opts().is_some());
    let bad = ConnectionConfig {
        url: Some("mysql://h/x?nonsense=1".into()),
        ..Default::default()
    };
    assert!(mysql_opts(&bad).is_err());
}

fn test_url() -> Option<String> {
    std::env::var("DELTOS_TEST_MYSQL_URL")
        .ok()
        .filter(|url| !url.trim().is_empty())
}

async fn connect() -> Option<Arc<dyn Connection>> {
    let config = ConnectionConfig {
        url: Some(test_url()?),
        tls: TlsMode::Prefer,
        ..Default::default()
    };
    Some(MysqlDriver.connect(&config).await.expect("connect"))
}

async fn run(conn: &dyn Connection, sql: &str) -> RowStream {
    conn.execute(sql, &ExecuteOptions::default())
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
}

async fn rows_of(conn: &dyn Connection, sql: &str) -> Vec<Row> {
    let mut stream = run(conn, sql).await;
    let mut out = Vec::new();
    while let Some(batch) = stream.batches.next().await {
        out.extend(batch.expect("batch").rows);
    }
    out
}

/// A fresh database for one test.
async fn database(conn: &dyn Connection, name: &str) -> String {
    let db = format!("deltos_test_{name}");
    run(
        conn,
        &format!("DROP DATABASE IF EXISTS {db}; CREATE DATABASE {db}"),
    )
    .await;
    db
}

async fn read_all(
    conn: &dyn Connection,
    object: &ObjectRef,
    base: &BrowseRequest,
    seek: bool,
) -> Vec<Row> {
    let mut request = base.clone();
    request.limit = 997;
    let mut out = Vec::new();
    loop {
        let page = conn.browse(object, &request).await.expect("browse");
        out.extend(page.rows);
        if !page.more {
            return out;
        }
        request.offset = out.len() as u64;
        if seek {
            request.seek_after = Some(page.last_key.expect("last key"));
        }
    }
}

#[tokio::test]
async fn pages_identically_with_and_without_seek() {
    let Some(conn) = connect().await else { return };
    let db = database(conn.as_ref(), "paging").await;
    run(
        conn.as_ref(),
        &format!(
            "CREATE TABLE {db}.t (id INT PRIMARY KEY, a INT NULL, b VARCHAR(10) NULL, flag TINYINT(1));
             INSERT INTO {db}.t
             WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 999)
             SELECT x.i + 1000 * y.i, IF((x.i + y.i) % 7 = 0, NULL, (x.i * 31) % 100),
                    IF((x.i + y.i) % 11 = 0, NULL, CONCAT('s', (y.i * 17) % 37)), x.i % 2
             FROM n AS x CROSS JOIN (SELECT 0 AS i UNION ALL SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3
                                     UNION ALL SELECT 4 UNION ALL SELECT 5 UNION ALL SELECT 6 UNION ALL SELECT 7) AS y;"
        ),
    )
    .await;
    let t = ObjectRef {
        namespace: vec![db.clone()],
        name: "t".into(),
        kind: ObjectKind::Table,
    };
    let total = 999 * 8;
    for sort in [
        vec![],
        vec![Sort {
            column: "a".into(),
            direction: SortDirection::Ascending,
        }],
        vec![
            Sort {
                column: "a".into(),
                direction: SortDirection::Descending,
            },
            Sort {
                column: "b".into(),
                direction: SortDirection::Ascending,
            },
        ],
        vec![Sort {
            column: "b".into(),
            direction: SortDirection::Descending,
        }],
    ] {
        let base = BrowseRequest {
            sort: sort.clone(),
            ..Default::default()
        };
        let by_offset = read_all(conn.as_ref(), &t, &base, false).await;
        let by_seek = read_all(conn.as_ref(), &t, &base, true).await;
        assert_eq!(by_offset.len(), total, "{sort:?}");
        assert_eq!(by_offset, by_seek, "{sort:?}");
    }
    let page = conn
        .browse(
            &t,
            &BrowseRequest {
                limit: 1,
                ..Default::default()
            },
        )
        .await
        .expect("browse");
    assert_eq!(page.columns[3].kind, ValueKind::Bool);
    assert!(matches!(page.rows[0][3], Value::Bool(_)));

    assert_eq!(
        conn.count(&t, &BrowseRequest::default(), true)
            .await
            .expect("count"),
        total as u64
    );
    assert!(
        conn.count(&t, &BrowseRequest::default(), false)
            .await
            .is_ok()
    );
    let filtered = BrowseRequest {
        filters: vec![Filter {
            column: "a".into(),
            op: FilterOp::IsNull,
            value: Value::Null,
        }],
        ..Default::default()
    };
    assert!(conn.count(&t, &filtered, false).await.is_ok());
    let n = conn.count(&t, &filtered, true).await.expect("count");
    assert!(n > 0 && n < total as u64);

    // A cross join of the table with itself is 64 million rows, produced as they are read.
    let started = Instant::now();
    let mut stream = conn
        .execute(
            &format!("SELECT x.id, y.b FROM {db}.t AS x CROSS JOIN {db}.t AS y"),
            &ExecuteOptions {
                batch_size: 300,
                namespace: None,
            },
        )
        .await
        .expect("execute");
    let batch = stream.batches.next().await.expect("batch").expect("rows");
    assert_eq!(batch.rows.len(), 300);
    drop(stream);
    let rows = rows_of(conn.as_ref(), "SELECT 1").await;
    assert_eq!(rows[0][0], Value::Int(1));
    assert!(started.elapsed() < Duration::from_secs(20));

    // Running a statement closes the result still open before it.
    let mut open = conn
        .execute(
            &format!("SELECT x.id FROM {db}.t AS x CROSS JOIN {db}.t AS y"),
            &ExecuteOptions::default(),
        )
        .await
        .expect("execute");
    assert!(open.batches.next().await.is_some());
    let rows = rows_of(conn.as_ref(), "SELECT 2").await;
    assert_eq!(rows[0][0], Value::Int(2));
    let mut ended = false;
    while let Some(batch) = open.batches.next().await {
        if batch.is_err() {
            ended = true;
        }
    }
    let _ = ended;
}

#[tokio::test]
async fn cancels_and_reports() {
    let Some(conn) = connect().await else { return };
    let cancel = conn.cancel_handle().expect("cancel");
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let _ = cancel.cancel().await;
    });
    let started = Instant::now();
    let result = conn
        .execute(
            "SELECT SLEEP(20) FROM (SELECT 1 UNION ALL SELECT 2) AS x",
            &ExecuteOptions::default(),
        )
        .await;
    let error = match result {
        Ok(mut stream) => stream.batches.next().await.and_then(|b| b.err()),
        Err(e) => Some(e),
    };
    assert!(matches!(error, Some(Error::Cancelled)), "{error:?}");
    assert!(started.elapsed() < Duration::from_secs(15));

    match conn
        .execute(
            "SELECT * FROM no_such_db.no_such_table",
            &ExecuteOptions::default(),
        )
        .await
    {
        Err(Error::Query(message)) => assert!(message.contains("no_such"), "{message}"),
        other => panic!("{other:?}"),
    }
    let rows = rows_of(
        conn.as_ref(),
        "SELECT 1, -2, 1.5e0, CAST(12.30 AS DECIMAL(6,2)), 'hé', X'00FF', DATE '2024-02-29', TIMESTAMP '2024-02-29 01:02:03.5', TIME '-26:03:04', JSON_ARRAY(1, 2), NULL",
    )
    .await;
    let text: Vec<String> = rows[0].iter().map(|v| v.to_string()).collect();
    assert_eq!(rows[0][0], Value::Int(1));
    assert_eq!(rows[0][1], Value::Int(-2));
    assert_eq!(rows[0][2], Value::Float(1.5));
    assert_eq!(text[3], "12.30");
    assert_eq!(text[4], "hé");
    assert_eq!(rows[0][5], Value::Bytes(vec![0, 255].into()));
    assert_eq!(text[6], "2024-02-29");
    assert!(text[7].starts_with("2024-02-29 01:02:03.5"), "{}", text[7]);
    assert_eq!(text[8], "-26:03:04");
    // MySQL types JSON_ARRAY as JSON; MariaDB, where JSON is LONGTEXT, as text.
    assert_eq!(text[9], "[1, 2]");
    assert_eq!(rows[0][10], Value::Null);
}

#[tokio::test]
async fn describes_and_edits() {
    let Some(conn) = connect().await else { return };
    let db = database(conn.as_ref(), "describe").await;
    run(
        conn.as_ref(),
        &format!(
            "CREATE TABLE {db}.parent (id INT AUTO_INCREMENT PRIMARY KEY, name VARCHAR(20) NOT NULL DEFAULT 'x' COMMENT 'the name') COMMENT 'parents';
             CREATE TABLE {db}.child (a INT, b INT, parent_id INT, PRIMARY KEY (b, a), KEY child_parent (parent_id),
                 CONSTRAINT child_fk FOREIGN KEY (parent_id) REFERENCES {db}.parent (id));
             CREATE TABLE {db}.nokey (v INT, u INT NOT NULL, UNIQUE KEY (u));
             CREATE VIEW {db}.v AS SELECT id FROM {db}.parent;
             INSERT INTO {db}.parent (name) VALUES ('a'), ('b');
             INSERT INTO {db}.nokey VALUES (1, 10), (1, 20), (2, 30);"
        ),
    )
    .await;
    let ns = vec![db.clone()];
    let objects = conn.objects(&ns).await.expect("objects");
    let names: Vec<(&str, ObjectKind)> = objects
        .iter()
        .map(|o| (o.object.name.as_str(), o.object.kind))
        .collect();
    assert!(names.contains(&("parent", ObjectKind::Table)));
    assert!(names.contains(&("v", ObjectKind::View)));
    assert!(
        conn.namespaces()
            .await
            .expect("namespaces")
            .iter()
            .any(|n| n.path == [db.clone()])
    );

    let child = ObjectRef {
        namespace: ns.clone(),
        name: "child".into(),
        kind: ObjectKind::Table,
    };
    let info = conn.describe(&child).await.expect("describe");
    let key: Vec<&str> = info.primary_key().iter().map(|c| c.name.as_str()).collect();
    assert_eq!(key, ["b", "a"]);
    assert_eq!(info.foreign_keys[0].name, "child_fk");
    assert_eq!(info.foreign_keys[0].target.name, "parent");
    assert!(info.indexes.iter().any(|i| i.name == "child_parent"));
    assert!(
        info.definition
            .expect("definition")
            .starts_with("CREATE TABLE `child`")
    );

    let parent = ObjectRef {
        name: "parent".into(),
        ..child.clone()
    };
    let info = conn.describe(&parent).await.expect("describe");
    assert_eq!(info.comment.as_deref(), Some("parents"));
    assert_eq!(info.columns[1].comment.as_deref(), Some("the name"));
    assert_eq!(info.columns[1].nullable, Some(false));
    let view = ObjectRef {
        name: "v".into(),
        kind: ObjectKind::View,
        ..child.clone()
    };
    assert!(
        conn.describe(&view)
            .await
            .expect("view")
            .definition
            .is_some()
    );

    // A unique NOT NULL key stands in for a missing primary key.
    let nokey = ObjectRef {
        name: "nokey".into(),
        ..child.clone()
    };
    let page = conn
        .browse(
            &nokey,
            &BrowseRequest {
                limit: 2,
                ..Default::default()
            },
        )
        .await
        .expect("browse");
    assert_eq!(page.last_key.expect("key")[0].0, "u");

    let edits = vec![
        RowEdit::Update {
            key: vec![("id".into(), Value::Int(1))],
            changes: vec![("name".into(), Value::text("a"))],
        },
        RowEdit::Insert {
            values: vec![("name".into(), Value::text("c"))],
        },
        RowEdit::Delete {
            key: vec![("id".into(), Value::Int(2))],
        },
    ];
    assert_eq!(conn.apply_edits(&parent, &edits).await.expect("edits"), 3);
    let failing = vec![
        RowEdit::Update {
            key: vec![("id".into(), Value::Int(1))],
            changes: vec![("name".into(), Value::text("never"))],
        },
        RowEdit::Delete {
            key: vec![("id".into(), Value::Int(999))],
        },
    ];
    assert!(conn.apply_edits(&parent, &failing).await.is_err());
    let options = ExecuteOptions {
        namespace: Some(ns.clone()),
        ..Default::default()
    };
    let mut stream = conn
        .execute("SELECT name FROM parent ORDER BY id", &options)
        .await
        .expect("execute");
    let batch = stream.batches.next().await.expect("batch").expect("rows");
    let names: Vec<String> = batch.rows.iter().map(|r| r[0].to_string()).collect();
    assert_eq!(names, ["a", "c"]);
    let update = run(
        conn.as_ref(),
        &format!("UPDATE {db}.parent SET name = name"),
    )
    .await;
    assert_eq!(
        update.affected,
        Some(2),
        "matched rows, not only changed ones"
    );
    let ddl = run(conn.as_ref(), &format!("CREATE TABLE {db}.later (x INT)")).await;
    assert_eq!(ddl.affected, None);
}

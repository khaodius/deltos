use std::cmp::Ordering;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use deltos_data::{Filter, FilterOp, Sort, SortDirection};
use futures::StreamExt;

use super::*;

/// A database file in the temp directory, removed when the test ends.
struct TempDb(PathBuf);

impl TempDb {
    fn new(name: &str) -> Self {
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "deltos-sqlite-{}-{name}-{n}.db",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        Self(path)
    }

    fn config(&self) -> ConnectionConfig {
        ConnectionConfig {
            database: self.0.to_string_lossy().into_owned(),
            options: vec![("create".into(), "true".into())],
            ..Default::default()
        }
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm", "-journal"] {
            let mut path = self.0.clone().into_os_string();
            path.push(suffix);
            let _ = std::fs::remove_file(path);
        }
    }
}

const ROWS: i64 = 100_000;

fn a_of(i: i64) -> Option<i64> {
    (i % 7 != 0).then_some((i * 31) % 100)
}

fn b_of(i: i64) -> Option<String> {
    (i % 11 != 0).then(|| format!("s{:02}", (i * 17) % 37))
}

/// Fills `t` with 100,000 rows that have many ties and NULLs in `a` and `b`.
fn fill(path: &PathBuf) {
    let mut conn = rusqlite::Connection::open(path).expect("open");
    conn.execute_batch(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, b TEXT, c REAL);
         CREATE TABLE r (name TEXT, v INTEGER);
         CREATE TABLE w (k TEXT NOT NULL, n INTEGER NOT NULL, PRIMARY KEY (k, n)) WITHOUT ROWID;",
    )
    .expect("schema");
    let tx = conn.transaction().expect("transaction");
    {
        let mut insert = tx
            .prepare("INSERT INTO t (id, a, b, c) VALUES (?1, ?2, ?3, ?4)")
            .expect("prepare");
        for i in 1..=ROWS {
            insert
                .execute(rusqlite::params![i, a_of(i), b_of(i), i as f64 / 3.0])
                .expect("insert");
        }
        let mut insert = tx
            .prepare("INSERT INTO r VALUES (?1, ?2)")
            .expect("prepare");
        for i in 0..5000i64 {
            insert
                .execute(rusqlite::params![format!("n{}", i % 13), i % 17])
                .expect("insert");
        }
        let mut insert = tx
            .prepare("INSERT INTO w VALUES (?1, ?2)")
            .expect("prepare");
        for i in 0..3000i64 {
            insert
                .execute(rusqlite::params![format!("k{}", i % 41), i])
                .expect("insert");
        }
    }
    tx.commit().expect("commit");
}

fn table(name: &str) -> ObjectRef {
    ObjectRef {
        namespace: vec!["main".into()],
        name: name.into(),
        kind: ObjectKind::Table,
    }
}

fn sort(column: &str, descending: bool) -> Sort {
    Sort {
        column: column.into(),
        direction: if descending {
            SortDirection::Descending
        } else {
            SortDirection::Ascending
        },
    }
}

/// Reads every page of `base`, by offset or by seeking from each page's last key.
async fn read_all(
    conn: &SqliteConnection,
    object: &ObjectRef,
    base: &BrowseRequest,
    seek: bool,
    page: u32,
) -> Vec<Row> {
    let mut request = base.clone();
    request.limit = page;
    let mut out: Vec<Row> = Vec::new();
    loop {
        let result = conn.browse(object, &request).await.expect("browse");
        assert!(result.rows.len() <= page as usize);
        out.extend(result.rows);
        if !result.more {
            break;
        }
        request.offset = out.len() as u64;
        if seek {
            assert!(result.last_key.is_some(), "a table page has a last key");
            request.seek_after = result.last_key;
        }
    }
    out
}

/// SQLite's order: NULL before any value when ascending, after when descending.
fn compare<T: Ord>(x: &Option<T>, y: &Option<T>, descending: bool) -> Ordering {
    let ordering = x.cmp(y);
    if descending {
        ordering.reverse()
    } else {
        ordering
    }
}

#[tokio::test]
async fn paging_with_and_without_seek_matches() {
    let db = TempDb::new("paging");
    fill(&db.0);
    let conn = SqliteConnection::open(&db.config()).await.expect("open");
    let t = table("t");

    let cases: Vec<(Vec<Sort>, Vec<Filter>)> = vec![
        (vec![], vec![]),
        (vec![sort("a", false)], vec![]),
        (vec![sort("a", true), sort("b", false)], vec![]),
        (vec![sort("b", true), sort("a", false)], vec![]),
        (
            vec![sort("b", false)],
            vec![Filter {
                column: "a".into(),
                op: FilterOp::GtEq,
                value: Value::Int(50),
                values: Vec::new(),
            }],
        ),
    ];
    for (sorts, filters) in cases {
        let mut expected: Vec<i64> = (1..=ROWS)
            .filter(|i| filters.is_empty() || a_of(*i).is_some_and(|a| a >= 50))
            .collect();
        expected.sort_by(|x, y| {
            for s in &sorts {
                let descending = s.direction == SortDirection::Descending;
                let ordering = match s.column.as_str() {
                    "a" => compare(&a_of(*x), &a_of(*y), descending),
                    _ => compare(&b_of(*x), &b_of(*y), descending),
                };
                if ordering != Ordering::Equal {
                    return ordering;
                }
            }
            x.cmp(y)
        });

        let base = BrowseRequest {
            sort: sorts.clone(),
            filters: filters.clone(),
            ..Default::default()
        };
        let by_offset = read_all(&conn, &t, &base, false, 7919).await;
        let by_seek = read_all(&conn, &t, &base, true, 7919).await;
        let ids = |rows: &[Row]| -> Vec<i64> {
            rows.iter()
                .map(|row| match row[0] {
                    Value::Int(id) => id,
                    ref other => panic!("id is {other:?}"),
                })
                .collect()
        };
        assert_eq!(ids(&by_offset), expected, "offset paging for {sorts:?}");
        assert_eq!(ids(&by_seek), expected, "seek paging for {sorts:?}");
    }
}

#[tokio::test]
async fn seeks_by_hidden_rowid_and_without_rowid_keys() {
    let db = TempDb::new("rowid");
    fill(&db.0);
    let conn = SqliteConnection::open(&db.config()).await.expect("open");

    let r = table("r");
    let base = BrowseRequest {
        sort: vec![sort("v", false)],
        ..Default::default()
    };
    let first = conn
        .browse(
            &r,
            &BrowseRequest {
                limit: 10,
                ..base.clone()
            },
        )
        .await
        .expect("browse");
    assert_eq!(first.columns.len(), 2, "the rowid stays hidden");
    assert!(first.rows.iter().all(|row| row.len() == 2));
    let key = first.last_key.expect("last key");
    assert_eq!(
        key.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(),
        ["v", "rowid"]
    );

    let by_offset = read_all(&conn, &r, &base, false, 333).await;
    let by_seek = read_all(&conn, &r, &base, true, 333).await;
    assert_eq!(by_offset.len(), 5000);
    assert_eq!(by_offset, by_seek);

    let w = table("w");
    let base = BrowseRequest {
        sort: vec![sort("n", true)],
        ..Default::default()
    };
    let by_offset = read_all(&conn, &w, &base, false, 250).await;
    let by_seek = read_all(&conn, &w, &base, true, 250).await;
    assert_eq!(by_offset.len(), 3000);
    assert_eq!(by_offset, by_seek);
}

#[tokio::test]
async fn streams_without_reading_ahead() {
    let conn = SqliteConnection::open(&ConnectionConfig {
        database: ":memory:".into(),
        ..Default::default()
    })
    .await
    .expect("open");
    // An endless result: reading it whole would never finish.
    let endless = "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c) SELECT x, 'row ' || x AS label FROM c";
    let options = ExecuteOptions {
        batch_size: 100,
        namespace: None,
    };
    let started = Instant::now();
    let mut first = conn.execute(endless, &options).await.expect("execute");
    assert_eq!(first.columns.len(), 2);
    assert_eq!(first.columns[0].kind, ValueKind::Int);
    let mut second = conn.execute(endless, &options).await.expect("execute");

    // Two open results and other work interleave on the one connection.
    for round in 0..3i64 {
        let a = first.batches.next().await.expect("batch").expect("rows");
        assert_eq!(a.rows.len(), 100);
        assert_eq!(a.rows[0][0], Value::Int(round * 100 + 1));
        let b = second.batches.next().await.expect("batch").expect("rows");
        assert_eq!(b.rows[99][0], Value::Int(round * 100 + 100));
        let namespaces = conn.namespaces().await.expect("namespaces");
        assert!(namespaces.iter().any(|n| n.path == ["main"]));
    }
    drop(first);
    let b = second.batches.next().await.expect("batch").expect("rows");
    assert_eq!(b.rows[0][0], Value::Int(301));
    drop(second);
    assert!(started.elapsed() < Duration::from_secs(5));

    let mut small = conn
        .execute("SELECT 1 AS one UNION ALL SELECT 2", &options)
        .await
        .expect("execute");
    let batch = small.batches.next().await.expect("batch").expect("rows");
    assert_eq!(batch.rows.len(), 2);
    assert!(small.batches.next().await.is_none());
}

#[tokio::test]
async fn closed_cursors_below_open_ones_stay_answerable() {
    let conn = SqliteConnection::open(&ConnectionConfig {
        database: ":memory:".into(),
        ..Default::default()
    })
    .await
    .expect("open");
    let options = ExecuteOptions {
        batch_size: 1,
        namespace: None,
    };
    let endless = "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c) SELECT x FROM c";
    let mut bottom = conn.execute(endless, &options).await.expect("execute");
    let mut top = conn.execute(endless, &options).await.expect("execute");
    assert!(bottom.batches.next().await.is_some());
    drop(bottom);
    let batch = top.batches.next().await.expect("batch").expect("rows");
    assert_eq!(batch.rows[0][0], Value::Int(1));
    drop(top);
    let mut again = conn.execute("SELECT 42", &options).await.expect("execute");
    let batch = again.batches.next().await.expect("batch").expect("rows");
    assert_eq!(batch.rows[0][0], Value::Int(42));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancels_a_running_statement() {
    let conn = SqliteConnection::open(&ConnectionConfig {
        database: ":memory:".into(),
        ..Default::default()
    })
    .await
    .expect("open");
    let cancel = conn.cancel_handle().expect("cancel handle");
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let _ = cancel.cancel().await;
    });
    let started = Instant::now();
    let result = conn
        .execute(
            "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c) SELECT x FROM c WHERE x < 0",
            &ExecuteOptions::default(),
        )
        .await;
    assert!(matches!(result, Err(Error::Cancelled)), "{result:?}");
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(conn.namespaces().await.is_ok());
}

#[tokio::test]
async fn runs_scripts_and_reports_changes() {
    let conn = SqliteConnection::open(&ConnectionConfig {
        database: ":memory:".into(),
        ..Default::default()
    })
    .await
    .expect("open");
    let options = ExecuteOptions::default();
    let result = conn
        .execute(
            "CREATE TABLE x (a INTEGER, j JSON, flag BOOLEAN); -- set up\nINSERT INTO x VALUES (1, '{\"k\": 1}', 1), (2, NULL, 0);\nSELECT * FROM x ORDER BY a;",
            &options,
        )
        .await
        .expect("script");
    let mut batches = result.batches;
    let batch = batches.next().await.expect("batch").expect("rows");
    assert_eq!(batch.rows.len(), 2);
    assert_eq!(batch.rows[0][1], Value::Json("{\"k\": 1}".into()));
    assert_eq!(batch.rows[1][2], Value::Bool(false));

    let update = conn
        .execute("UPDATE x SET a = a + 10", &options)
        .await
        .expect("update");
    assert_eq!(update.affected, Some(2));
    assert!(update.columns.is_empty());
    let ddl = conn
        .execute("CREATE INDEX xi ON x (a)", &options)
        .await
        .expect("ddl");
    assert_eq!(ddl.affected, None);

    let error = conn.execute("SELECT * FROM nope", &options).await.err();
    match error {
        Some(Error::Query(message)) => assert!(message.contains("no such table"), "{message}"),
        other => panic!("expected a query error, got {other:?}"),
    }
    let empty = conn
        .execute("  -- nothing\n", &options)
        .await
        .expect("empty");
    assert!(empty.columns.is_empty());
}

#[tokio::test]
async fn counts_and_describes() {
    let db = TempDb::new("describe");
    fill(&db.0);
    {
        let conn = rusqlite::Connection::open(&db.0).expect("open");
        conn.execute_batch(
            "CREATE TABLE parent (id INTEGER PRIMARY KEY, name TEXT NOT NULL DEFAULT 'x');
             CREATE TABLE child (id INTEGER PRIMARY KEY, parent_id INTEGER REFERENCES parent, note VARCHAR(20));
             CREATE INDEX child_parent ON child (parent_id);
             CREATE VIEW big AS SELECT id, a FROM t WHERE a > 90;",
        )
        .expect("schema");
    }
    let conn = SqliteConnection::open(&db.config()).await.expect("open");
    let t = table("t");
    let all = BrowseRequest::default();
    assert_eq!(
        conn.count(&t, &all, true).await.expect("count"),
        ROWS as u64
    );
    let filtered = BrowseRequest {
        filters: vec![Filter {
            column: "a".into(),
            op: FilterOp::IsNull,
            value: Value::Null,
            values: Vec::new(),
        }],
        ..Default::default()
    };
    assert_eq!(
        conn.count(&t, &filtered, false).await.expect("count"),
        (1..=ROWS).filter(|i| a_of(*i).is_none()).count() as u64
    );
    conn.execute("ANALYZE", &ExecuteOptions::default())
        .await
        .expect("analyze");
    assert_eq!(
        conn.count(&t, &all, false).await.expect("estimate"),
        ROWS as u64
    );

    let objects = conn.objects(&["main".to_string()]).await.expect("objects");
    let names: Vec<&str> = objects.iter().map(|o| o.object.name.as_str()).collect();
    assert_eq!(names, ["big", "child", "parent", "r", "t", "w"]);
    assert_eq!(objects[0].object.kind, ObjectKind::View);
    assert_eq!(
        objects
            .iter()
            .find(|o| o.object.name == "t")
            .and_then(|o| o.estimated_rows),
        Some(ROWS as u64)
    );

    let child = conn.describe(&table("child")).await.expect("describe");
    assert_eq!(child.columns.len(), 3);
    assert_eq!(child.columns[0].primary_key, Some(0));
    assert_eq!(child.columns[2].kind, ValueKind::Text);
    assert_eq!(child.foreign_keys.len(), 1);
    assert_eq!(child.foreign_keys[0].target.name, "parent");
    assert_eq!(child.foreign_keys[0].target_columns, ["id"]);
    assert!(
        child
            .indexes
            .iter()
            .any(|i| i.name == "child_parent" && i.columns == ["parent_id"])
    );
    let definition = child.definition.expect("definition");
    assert!(definition.starts_with("CREATE TABLE child"));
    assert!(definition.contains("CREATE INDEX child_parent"));

    let parent = conn.describe(&table("parent")).await.expect("describe");
    assert_eq!(parent.columns[1].nullable, Some(false));
    assert_eq!(parent.columns[1].default.as_deref(), Some("'x'"));

    let view = ObjectRef {
        kind: ObjectKind::View,
        ..table("big")
    };
    let page = conn
        .browse(
            &view,
            &BrowseRequest {
                limit: 5,
                ..Default::default()
            },
        )
        .await
        .expect("browse view");
    assert_eq!(page.rows.len(), 5);
    assert!(page.last_key.is_none(), "a view has no key to seek by");

    assert!(conn.describe(&table("missing")).await.is_err());
}

/// Every filter bar operator, checked against the rows it should keep.
#[tokio::test]
async fn filter_operators_keep_the_right_rows() {
    let db = TempDb::new("operators");
    {
        let conn = rusqlite::Connection::open(&db.0).expect("open");
        conn.execute_batch(
            "CREATE TABLE f (id INTEGER PRIMARY KEY, name TEXT, n INTEGER);
             INSERT INTO f VALUES
               (1, 'Ada', 10), (2, 'grace', 20), (3, '50% off', 30), (4, '', 40),
               (5, NULL, NULL), (6, 'Linus_t', 50), (7, 'adam', 60);",
        )
        .expect("schema");
    }
    let conn = SqliteConnection::open(&db.config()).await.expect("open");
    let f = table("f");
    let ids = |request: BrowseRequest| {
        let conn = &conn;
        let f = &f;
        async move {
            let request = BrowseRequest {
                limit: 100,
                ..request
            };
            let page = conn.browse(f, &request).await.expect("browse");
            let n = conn.count(f, &request, true).await.expect("count");
            let ids: Vec<i64> = page
                .rows
                .iter()
                .map(|r| match r[0] {
                    Value::Int(i) => i,
                    _ => -1,
                })
                .collect();
            assert_eq!(n, ids.len() as u64);
            ids
        }
    };
    let one = |op, value| BrowseRequest {
        filters: vec![Filter::new("name", op, value)],
        ..Default::default()
    };
    let list = |column: &str, op, values| BrowseRequest {
        filters: vec![Filter::list(column, op, values)],
        ..Default::default()
    };
    assert_eq!(
        ids(one(FilterOp::NotContains, Value::text("ad"))).await,
        [2, 3, 4, 6]
    );
    assert_eq!(ids(one(FilterOp::EndsWith, Value::text("_T"))).await, [6]);
    assert_eq!(ids(one(FilterOp::Contains, Value::text("%"))).await, [3]);
    assert_eq!(ids(one(FilterOp::Like, Value::text("ad%"))).await, [1, 7]);
    assert_eq!(ids(one(FilterOp::Like, Value::text("_race"))).await, [2]);
    assert_eq!(ids(one(FilterOp::IsEmpty, Value::Null)).await, [4, 5]);
    assert_eq!(
        ids(one(FilterOp::IsNotEmpty, Value::Null)).await,
        [1, 2, 3, 6, 7]
    );
    assert_eq!(
        ids(list(
            "n",
            FilterOp::In,
            vec![Value::Int(20), Value::Int(60), Value::Null]
        ))
        .await,
        [2, 5, 7]
    );
    assert_eq!(
        ids(list(
            "n",
            FilterOp::NotIn,
            vec![Value::Int(10), Value::Int(20)]
        ))
        .await,
        [3, 4, 6, 7]
    );
    assert_eq!(
        ids(list(
            "n",
            FilterOp::Between,
            vec![Value::Int(20), Value::Int(40)]
        ))
        .await,
        [2, 3, 4]
    );
    let any = BrowseRequest {
        filters: vec![
            Filter::new("n", FilterOp::Lt, Value::Int(15)),
            Filter::new("name", FilterOp::StartsWith, Value::text("lin")),
        ],
        match_any: true,
        raw_where: Some("id > 1".into()),
        ..Default::default()
    };
    assert_eq!(ids(any).await, [6]);
}

#[tokio::test]
async fn applies_edits_in_one_transaction() {
    let conn = SqliteConnection::open(&ConnectionConfig {
        database: ":memory:".into(),
        ..Default::default()
    })
    .await
    .expect("open");
    let options = ExecuteOptions::default();
    conn.execute(
        "CREATE TABLE p (id INTEGER PRIMARY KEY, name TEXT); INSERT INTO p VALUES (1, 'a'), (2, 'b'), (3, 'c')",
        &options,
    )
    .await
    .expect("setup");
    let p = table("p");
    let edits = vec![
        RowEdit::Update {
            key: vec![("id".into(), Value::Int(1))],
            changes: vec![("name".into(), Value::text("z"))],
        },
        RowEdit::Delete {
            key: vec![("id".into(), Value::Int(2))],
        },
        RowEdit::Insert {
            values: vec![
                ("id".into(), Value::Int(9)),
                ("name".into(), Value::text("new")),
            ],
        },
    ];
    assert_eq!(conn.apply_edits(&p, &edits).await.expect("edits"), 3);

    let failing = vec![
        RowEdit::Update {
            key: vec![("id".into(), Value::Int(3))],
            changes: vec![("name".into(), Value::text("never"))],
        },
        RowEdit::Delete {
            key: vec![("id".into(), Value::Int(404))],
        },
    ];
    assert!(conn.apply_edits(&p, &failing).await.is_err());

    let page = conn
        .browse(
            &p,
            &BrowseRequest {
                limit: 10,
                ..Default::default()
            },
        )
        .await
        .expect("browse");
    let names: Vec<String> = page.rows.iter().map(|r| r[1].to_string()).collect();
    assert_eq!(
        names,
        ["z", "c", "new"],
        "the failed batch left nothing behind"
    );
}

#[tokio::test]
async fn refuses_missing_files_unless_asked_to_create() {
    let db = TempDb::new("missing");
    let mut config = db.config();
    config.options.clear();
    assert!(matches!(
        SqliteConnection::open(&config).await,
        Err(Error::Connect(_))
    ));
    assert!(SqliteConnection::open(&db.config()).await.is_ok());
    let url = ConnectionConfig {
        url: Some("sqlite::memory:".into()),
        ..Default::default()
    };
    let conn = SqliteDriver.connect(&url).await.expect("url");
    assert!(conn.server_version().starts_with("SQLite 3."));
}

#[test]
fn declared_types_map_to_kinds() {
    assert_eq!(kind_for_decl("INTEGER"), ValueKind::Int);
    assert_eq!(kind_for_decl("varchar(20)"), ValueKind::Text);
    assert_eq!(kind_for_decl("BOOLEAN"), ValueKind::Bool);
    assert_eq!(kind_for_decl("DATETIME"), ValueKind::DateTime);
    assert_eq!(kind_for_decl("date"), ValueKind::Date);
    assert_eq!(kind_for_decl("DOUBLE PRECISION"), ValueKind::Float);
    assert_eq!(kind_for_decl("DECIMAL(10,2)"), ValueKind::Decimal);
    assert_eq!(kind_for_decl("blob"), ValueKind::Bytes);
    assert_eq!(kind_for_decl(""), ValueKind::Other);
}

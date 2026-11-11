//! Unit tests, and integration tests against the server named by
//! `DELTOS_TEST_POSTGRES_URL`. The integration tests pass without doing anything when
//! the variable is unset.

use std::time::{Duration, Instant};

use deltos_data::{Filter, FilterOp, ObjectKind, Sort, SortDirection, ValueKind};
use futures::StreamExt;

use super::*;

#[test]
fn cleans_urls() {
    let (url, mode) =
        clean_url("postgres://u@h/db?sslmode=verify-full&sslrootcert=x.pem&application_name=a");
    assert_eq!(url, "postgres://u@h/db?application_name=a");
    assert_eq!(mode, Some(TlsMode::VerifyFull));
    let (url, mode) = clean_url("postgres://u@h/db");
    assert_eq!(url, "postgres://u@h/db");
    assert_eq!(mode, None);
    let (url, mode) = clean_url("postgresql://h?sslmode=disable");
    assert_eq!(url, "postgresql://h");
    assert_eq!(mode, Some(TlsMode::Disable));
}

#[test]
fn builds_configs() {
    let config = ConnectionConfig {
        host: "db.local".into(),
        port: Some(6000),
        user: "me".into(),
        password: Some("secret".into()),
        database: "shop".into(),
        tls: TlsMode::VerifyFull,
        ..Default::default()
    };
    let (pg, mode) = pg_config(&config).expect("config");
    assert_eq!(mode, TlsMode::VerifyFull);
    assert_eq!(pg.get_user(), Some("me"));
    assert_eq!(pg.get_dbname(), Some("shop"));
    assert_eq!(pg.get_ports(), &[6000]);
    let url = ConnectionConfig {
        url: Some("postgres://other@h:1/x?sslmode=require".into()),
        password: Some("filled".into()),
        ..Default::default()
    };
    let (pg, mode) = pg_config(&url).expect("config");
    assert_eq!(mode, TlsMode::Require);
    assert_eq!(pg.get_user(), Some("other"));
    assert_eq!(pg.get_password(), Some(&b"filled"[..]));
    let bad = ConnectionConfig {
        url: Some("postgres://h:notaport/x".into()),
        ..Default::default()
    };
    assert!(pg_config(&bad).is_err());
}

#[test]
fn reads_plan_rows() {
    let json = r#"[{"Plan": {"Node Type": "Aggregate", "Plan Rows": 1, "Plans": [{"Node Type": "Seq Scan", "Plan Rows": 1234}]}}]"#;
    assert_eq!(plan_rows(json), Some(1234));
    assert_eq!(plan_rows("[]"), None);
    assert_eq!(plan_rows("nonsense"), None);
}

fn test_url() -> Option<String> {
    std::env::var("DELTOS_TEST_POSTGRES_URL")
        .ok()
        .filter(|url| !url.trim().is_empty())
}

async fn connect() -> Option<Arc<dyn Connection>> {
    let url = test_url()?;
    let config = ConnectionConfig {
        url: Some(url),
        tls: TlsMode::Prefer,
        ..Default::default()
    };
    Some(PostgresDriver.connect(&config).await.expect("connect"))
}

/// A fresh schema for one test, dropped at the start of the next run.
async fn schema(conn: &dyn Connection, name: &str) -> String {
    let schema = format!("deltos_test_{name}");
    let sql = format!("DROP SCHEMA IF EXISTS {schema} CASCADE; CREATE SCHEMA {schema}");
    conn.execute(&sql, &ExecuteOptions::default())
        .await
        .expect("schema");
    schema
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
    let schema = schema(conn.as_ref(), "paging").await;
    run(
        conn.as_ref(),
        &format!(
            "CREATE TABLE {schema}.t (id int PRIMARY KEY, a int, b text);
             INSERT INTO {schema}.t SELECT i, CASE WHEN i % 7 = 0 THEN NULL ELSE (i * 31) % 100 END,
                    CASE WHEN i % 11 = 0 THEN NULL ELSE 's' || ((i * 17) % 37) END
             FROM generate_series(1, 20000) AS i;
             CREATE TABLE {schema}.nokey (v int, w text);
             INSERT INTO {schema}.nokey SELECT i % 13, 'x' FROM generate_series(1, 3000) AS i;"
        ),
    )
    .await;
    let t = ObjectRef {
        namespace: vec![schema.clone()],
        name: "t".into(),
        kind: ObjectKind::Table,
    };
    let sorts = [
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
    ];
    for sort in sorts {
        let base = BrowseRequest {
            sort: sort.clone(),
            filters: vec![Filter {
                column: "id".into(),
                op: FilterOp::Gt,
                value: Value::Int(10),
                values: Vec::new(),
            }],
            ..Default::default()
        };
        let by_offset = read_all(conn.as_ref(), &t, &base, false).await;
        let by_seek = read_all(conn.as_ref(), &t, &base, true).await;
        assert_eq!(by_offset.len(), 19_990, "{sort:?}");
        assert_eq!(by_offset, by_seek, "{sort:?}");
        let mut ids: Vec<String> = by_seek.iter().map(|r| r[0].to_string()).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 19_990);
    }

    // Without a key the order ends in the hidden ctid.
    let nokey = ObjectRef {
        name: "nokey".into(),
        ..t.clone()
    };
    let base = BrowseRequest {
        sort: vec![Sort {
            column: "v".into(),
            direction: SortDirection::Ascending,
        }],
        ..Default::default()
    };
    let page = conn
        .browse(
            &nokey,
            &BrowseRequest {
                limit: 3,
                ..base.clone()
            },
        )
        .await
        .expect("browse");
    assert_eq!(page.columns.len(), 2);
    assert_eq!(page.rows[0].len(), 2);
    let key = page.last_key.expect("key");
    assert_eq!(key[1].0, "ctid");
    let by_offset = read_all(conn.as_ref(), &nokey, &base, false).await;
    let by_seek = read_all(conn.as_ref(), &nokey, &base, true).await;
    assert_eq!(by_offset.len(), 3000);
    assert_eq!(by_offset, by_seek);

    let exact = conn
        .count(&t, &BrowseRequest::default(), true)
        .await
        .expect("count");
    assert_eq!(exact, 20_000);
    run(conn.as_ref(), &format!("ANALYZE {schema}.t")).await;
    let estimate = conn
        .count(&t, &BrowseRequest::default(), false)
        .await
        .expect("estimate");
    assert!((15_000..=25_000).contains(&estimate), "{estimate}");
    let filtered = BrowseRequest {
        filters: vec![Filter {
            column: "b".into(),
            op: FilterOp::Contains,
            value: Value::text("S1"),
            values: Vec::new(),
        }],
        ..Default::default()
    };
    assert!(conn.count(&t, &filtered, false).await.is_ok());
    let n = conn.count(&t, &filtered, true).await.expect("count");
    assert!(n > 0 && n < 20_000);
}

#[tokio::test]
async fn streams_and_cancels() {
    let Some(conn) = connect().await else { return };
    let options = ExecuteOptions {
        batch_size: 500,
        namespace: None,
    };
    // Far more rows than could be held: reading ahead would never end. The series is in
    // the select list, where the server produces it row by row; in FROM it would be
    // materialized on the server first.
    let started = Instant::now();
    let mut stream = conn
        .execute(
            "SELECT generate_series(1, 2000000000) AS i, 'row' AS label",
            &options,
        )
        .await
        .expect("execute");
    let batch = stream.batches.next().await.expect("batch").expect("rows");
    assert_eq!(batch.rows.len(), 500);
    assert_eq!(batch.rows[0][0], Value::Int(1));
    drop(stream);
    // The next statement runs as soon as the abandoned one is cancelled.
    let rows = rows_of(conn.as_ref(), "SELECT 1").await;
    assert_eq!(rows[0][0], Value::Int(1));
    assert!(started.elapsed() < Duration::from_secs(20));

    let cancel = conn.cancel_handle().expect("cancel");
    let cancelling = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        cancel.cancel().await
    });
    let result = conn.execute("SELECT pg_sleep(30)", &options).await;
    let error = match result {
        Ok(mut stream) => stream.batches.next().await.and_then(|b| b.err()),
        Err(e) => Some(e),
    };
    assert!(matches!(error, Some(Error::Cancelled)), "{error:?}");
    // The request has been acted on once it returns, so it cannot reach what runs next.
    cancelling.await.expect("join").expect("cancel request");

    // Scripts run every statement; a statement run while a result is still open closes
    // that result instead of waiting for it.
    let mut open = run(
        conn.as_ref(),
        "CREATE TEMP TABLE IF NOT EXISTS seen (x int); SELECT generate_series(1, 100000000) AS two",
    )
    .await;
    assert_eq!(open.columns[0].name, "two");
    assert!(open.batches.next().await.is_some());
    let rows = rows_of(conn.as_ref(), "SELECT count(*) FROM seen").await;
    assert_eq!(rows[0][0], Value::Int(0));
    assert!(matches!(
        open.batches.next().await,
        Some(Err(Error::Other(_)))
    ));
    assert!(open.batches.next().await.is_none());
    // Notices come back with the result.
    let script = run(conn.as_ref(), "DO $$ BEGIN RAISE NOTICE 'hello'; END $$").await;
    assert!(
        script.messages.iter().any(|m| m.contains("hello")),
        "{:?}",
        script.messages
    );
    match conn.execute("SELECT * FROM no_such_table", &options).await {
        Err(Error::Query(message)) => assert!(message.contains("does not exist"), "{message}"),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn decodes_common_types() {
    let Some(conn) = connect().await else { return };
    let rows = rows_of(
        conn.as_ref(),
        "SELECT true, 42::int2, 7::int8, 1.5::float4, 12345.678::numeric, 'hi'::text, '\\x0102'::bytea,
                '2024-02-29'::date, '13:14:15.5'::time, '2024-02-29 01:02:03.25'::timestamp,
                '2024-02-29 01:02:03+00'::timestamptz, '1 year 2 days 00:00:03'::interval,
                'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11'::uuid, '{\"a\": [1, 2]}'::jsonb, '{1,NULL,3}'::int[],
                '10.1.0.0/16'::cidr, '[1,5)'::int4range, ROW(1, 'x'), '(1,2)'::point, NULL::text, 'NaN'::numeric",
    )
    .await;
    let row = &rows[0];
    let text: Vec<String> = row.iter().map(|v| v.to_string()).collect();
    assert_eq!(row[0], Value::Bool(true));
    assert_eq!(row[1], Value::Int(42));
    assert_eq!(row[3], Value::Float(1.5));
    assert_eq!(text[4], "12345.678");
    assert_eq!(row[6], Value::Bytes(vec![1, 2].into()));
    assert_eq!(text[7], "2024-02-29");
    assert_eq!(text[8], "13:14:15.5");
    assert_eq!(text[9], "2024-02-29 01:02:03.25");
    assert_eq!(text[10], "2024-02-29 01:02:03+00");
    assert_eq!(text[11], "1 year 2 days 00:00:03");
    assert_eq!(text[12], "a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11");
    assert_eq!(text[13], "{\"a\": [1, 2]}");
    assert_eq!(text[14], "[1,null,3]");
    assert_eq!(text[15], "10.1.0.0/16");
    assert_eq!(text[16], "[1,5)");
    assert_eq!(text[17], "[1,\"x\"]");
    assert_eq!(text[18], "(1,2)");
    assert_eq!(row[19], Value::Null);
    assert_eq!(text[20], "NaN");
}

#[tokio::test]
async fn describes_and_edits() {
    let Some(conn) = connect().await else { return };
    let schema = schema(conn.as_ref(), "describe").await;
    run(
        conn.as_ref(),
        &format!(
            "CREATE TYPE {schema}.mood AS ENUM ('ok', 'sad');
             CREATE TABLE {schema}.parent (id serial PRIMARY KEY, name text NOT NULL DEFAULT 'x', m {schema}.mood,
                 tags text[], tsv tsvector);
             COMMENT ON TABLE {schema}.parent IS 'the parents';
             CREATE TABLE {schema}.child (a int, b int, parent_id int REFERENCES {schema}.parent (id),
                 PRIMARY KEY (b, a));
             CREATE INDEX child_parent ON {schema}.child (parent_id);
             CREATE VIEW {schema}.v AS SELECT id FROM {schema}.parent;
             CREATE FUNCTION {schema}.f() RETURNS int LANGUAGE sql AS 'SELECT 1';
             INSERT INTO {schema}.parent (name, m, tags, tsv) VALUES ('a', 'ok', '{{x,y}}', 'cat dog'), ('b', NULL, NULL, NULL);"
        ),
    )
    .await;
    let ns = vec![schema.clone()];
    let objects = conn.objects(&ns).await.expect("objects");
    let names: Vec<(&str, ObjectKind)> = objects
        .iter()
        .map(|o| (o.object.name.as_str(), o.object.kind))
        .collect();
    assert!(names.contains(&("parent", ObjectKind::Table)));
    assert!(names.contains(&("v", ObjectKind::View)));
    assert!(names.contains(&("parent_id_seq", ObjectKind::Sequence)));
    assert!(names.contains(&("f", ObjectKind::Function)));
    let namespaces = conn.namespaces().await.expect("namespaces");
    assert!(namespaces.iter().any(|n| n.path.last() == Some(&schema)));

    let child = ObjectRef {
        namespace: ns.clone(),
        name: "child".into(),
        kind: ObjectKind::Table,
    };
    let info = conn.describe(&child).await.expect("describe");
    let key: Vec<&str> = info.primary_key().iter().map(|c| c.name.as_str()).collect();
    assert_eq!(key, ["b", "a"]);
    assert_eq!(info.foreign_keys[0].target.name, "parent");
    assert_eq!(info.foreign_keys[0].target_columns, ["id"]);
    assert!(
        info.indexes
            .iter()
            .any(|i| i.name == "child_parent" && i.columns == ["parent_id"])
    );
    let definition = info.definition.expect("definition");
    assert!(definition.contains("PRIMARY KEY (b, a)"), "{definition}");
    assert!(
        definition.contains("CREATE INDEX child_parent"),
        "{definition}"
    );

    let parent = ObjectRef {
        name: "parent".into(),
        ..child.clone()
    };
    let info = conn.describe(&parent).await.expect("describe");
    assert_eq!(info.comment.as_deref(), Some("the parents"));
    assert_eq!(info.columns[1].default.as_deref(), Some("'x'::text"));
    assert_eq!(info.columns[2].kind, ValueKind::Text);
    assert_eq!(info.columns[3].kind, ValueKind::Array);
    assert_eq!(info.referenced_by.len(), 1);
    assert_eq!(info.referenced_by[0].target.name, "child");
    assert_eq!(info.referenced_by[0].target.namespace.last(), Some(&schema));
    assert_eq!(info.referenced_by[0].columns, ["id"]);
    assert_eq!(info.referenced_by[0].target_columns, ["parent_id"]);
    let view = ObjectRef {
        name: "v".into(),
        kind: ObjectKind::View,
        ..child.clone()
    };
    let definition = conn
        .describe(&view)
        .await
        .expect("describe")
        .definition
        .expect("view");
    assert!(definition.starts_with("CREATE VIEW"), "{definition}");
    let function = ObjectRef {
        name: "f".into(),
        kind: ObjectKind::Function,
        ..child.clone()
    };
    let definition = conn
        .describe(&function)
        .await
        .expect("describe")
        .definition
        .expect("fn");
    assert!(
        definition.contains("CREATE OR REPLACE FUNCTION"),
        "{definition}"
    );

    // tsvector has no decoder and is read as text; the enum and the array decode.
    let page = conn
        .browse(
            &parent,
            &BrowseRequest {
                limit: 10,
                ..Default::default()
            },
        )
        .await
        .expect("browse");
    assert_eq!(page.rows.len(), 2);
    assert_eq!(page.rows[0][2], Value::text("ok"));
    assert_eq!(page.rows[0][3], Value::Json("[\"x\",\"y\"]".into()));
    assert_eq!(page.rows[0][4], Value::text("'cat' 'dog'"));

    let edits = vec![
        RowEdit::Update {
            key: vec![("id".into(), Value::Int(1))],
            changes: vec![
                ("name".into(), Value::text("z")),
                ("m".into(), Value::text("sad")),
            ],
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
    let rows = rows_of(
        conn.as_ref(),
        &format!("SELECT name, m FROM {schema}.parent ORDER BY id"),
    )
    .await;
    let names: Vec<String> = rows.iter().map(|r| r[0].to_string()).collect();
    assert_eq!(names, ["z", "c"]);
    assert_eq!(rows[0][1], Value::text("sad"));

    // The schema option sets the search path for statements.
    let options = ExecuteOptions {
        namespace: Some(vec![schema.clone()]),
        ..Default::default()
    };
    let mut stream = conn
        .execute("SELECT count(*) FROM parent", &options)
        .await
        .expect("execute");
    let batch = stream.batches.next().await.expect("batch").expect("rows");
    assert_eq!(batch.rows[0][0], Value::Int(2));
}

#[tokio::test]
async fn read_only_sessions_refuse_writes() {
    let Some(writer) = connect().await else {
        return;
    };
    let schema = schema(writer.as_ref(), "read_only").await;
    run(
        writer.as_ref(),
        &format!("CREATE TABLE {schema}.t (a int); INSERT INTO {schema}.t VALUES (1)"),
    )
    .await;
    let Some(url) = test_url() else {
        return;
    };
    let config = ConnectionConfig {
        url: Some(url),
        tls: TlsMode::Prefer,
        options: vec![("read_only".into(), "true".into())],
        ..Default::default()
    };
    let reader = PostgresDriver.connect(&config).await.expect("connect");
    // The query session and the metadata session are each made read-only.
    let delete = format!("DELETE FROM {schema}.t");
    let refused = reader
        .execute(&delete, &ExecuteOptions::default())
        .await
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
    assert!(refused.contains("read-only"), "{refused}");
    let object = ObjectRef {
        namespace: vec![schema.clone()],
        name: "t".into(),
        kind: ObjectKind::Table,
    };
    let edit = reader
        .apply_edits(
            &object,
            &[RowEdit::Insert {
                values: vec![("a".into(), Value::Int(2))],
            }],
        )
        .await;
    assert!(edit.is_err());
    assert_eq!(
        rows_of(reader.as_ref(), &format!("SELECT a FROM {schema}.t"))
            .await
            .len(),
        1
    );
    reader.close().await;
}

/// Every filter bar operator against a live server, values bound as text parameters.
#[tokio::test]
async fn filter_operators_keep_the_right_rows() {
    let Some(conn) = connect().await else { return };
    let schema = schema(conn.as_ref(), "operators").await;
    run(
        conn.as_ref(),
        &format!(
            "CREATE TABLE {schema}.f (id int PRIMARY KEY, name text, n int);
             INSERT INTO {schema}.f VALUES
               (1, 'Ada', 10), (2, 'grace', 20), (3, '50% off', 30), (4, '', 40),
               (5, NULL, NULL), (6, 'Linus_t', 50), (7, 'adam', 60);"
        ),
    )
    .await;
    let f = ObjectRef {
        namespace: vec![schema.clone()],
        name: "f".into(),
        kind: ObjectKind::Table,
    };
    let ids = async |filters: Vec<Filter>, match_any: bool| -> Vec<Value> {
        let request = BrowseRequest {
            limit: 100,
            filters,
            match_any,
            ..Default::default()
        };
        let page = conn.browse(&f, &request).await.expect("browse");
        let n = conn.count(&f, &request, true).await.expect("count");
        assert_eq!(n, page.rows.len() as u64);
        page.rows.iter().map(|r| r[0].clone()).collect()
    };
    let int = |ids: &[i64]| ids.iter().map(|i| Value::Int(*i)).collect::<Vec<_>>();
    let name = |op, text: &str| vec![Filter::new("name", op, Value::text(text))];
    assert_eq!(
        ids(name(FilterOp::NotContains, "ad"), false).await,
        int(&[2, 3, 4, 6])
    );
    assert_eq!(ids(name(FilterOp::EndsWith, "_T"), false).await, int(&[6]));
    assert_eq!(ids(name(FilterOp::Contains, "%"), false).await, int(&[3]));
    // LIKE keeps PostgreSQL's own case rules.
    assert_eq!(ids(name(FilterOp::Like, "ad%"), false).await, int(&[7]));
    assert_eq!(ids(name(FilterOp::IsEmpty, ""), false).await, int(&[4, 5]));
    assert_eq!(
        ids(name(FilterOp::IsNotEmpty, ""), false).await,
        int(&[1, 2, 3, 6, 7])
    );
    let n = |op, values: &[&str]| {
        vec![Filter::list(
            "n",
            op,
            values.iter().map(|v| Value::text(*v)).collect(),
        )]
    };
    assert_eq!(
        ids(n(FilterOp::In, &["20", "60"]), false).await,
        int(&[2, 7])
    );
    assert_eq!(
        ids(n(FilterOp::NotIn, &["10", "20"]), false).await,
        int(&[3, 4, 6, 7])
    );
    assert_eq!(
        ids(n(FilterOp::Between, &["20", "40"]), false).await,
        int(&[2, 3, 4])
    );
    let any = vec![
        Filter::new("n", FilterOp::Lt, Value::Int(15)),
        Filter::new("name", FilterOp::StartsWith, Value::text("lin")),
    ];
    assert_eq!(ids(any, true).await, int(&[1, 6]));
}

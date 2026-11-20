//! Tests against a live server, run when `DELTOS_TEST_MONGODB_URL` is set, for example
//! `mongodb://localhost:27017/deltos_test`. Each test works in a collection of its own
//! and drops it at the end. The parser, value mapping and query building are tested
//! without a server next to their code.

use std::sync::Arc;

use deltos_data::{
    BrowseRequest, Connection, ConnectionConfig, Driver, ExecuteOptions, Filter, FilterOp,
    ObjectKind, ObjectRef, RowEdit, Value, ValueKind,
};
use futures::StreamExt;

use super::*;

const ENV: &str = "DELTOS_TEST_MONGODB_URL";

async fn connect() -> Option<Arc<dyn Connection>> {
    let url = std::env::var(ENV).ok()?;
    let config = ConnectionConfig {
        url: Some(url),
        ..Default::default()
    };
    Some(MongoDriver.connect(&config).await.expect("connect"))
}

struct Fixture {
    conn: Arc<dyn Connection>,
    object: ObjectRef,
}

impl Fixture {
    async fn new(prefix: &str) -> Option<Self> {
        let conn = connect().await?;
        let namespaces = conn.namespaces().await.expect("namespaces");
        assert!(!namespaces.is_empty());
        conn.execute("db.runCommand({ping: 1})", &ExecuteOptions::default())
            .await
            .expect("ping");
        let url = std::env::var(ENV).ok()?;
        let options = client_options(&ConnectionConfig {
            url: Some(url),
            ..Default::default()
        })
        .await
        .expect("options");
        let database = options
            .default_database
            .unwrap_or_else(|| "deltos_test".to_string());
        let object = ObjectRef {
            namespace: vec![database],
            name: format!("{prefix}_{}", ObjectId::new().to_hex()),
            kind: ObjectKind::Collection,
        };
        Some(Self { conn, object })
    }

    fn options(&self) -> ExecuteOptions {
        ExecuteOptions {
            batch_size: 500,
            namespace: Some(self.object.namespace.clone()),
        }
    }

    async fn run(&self, statement: &str) -> (Vec<String>, Vec<Row>, Option<u64>) {
        let mut stream = self
            .conn
            .execute(statement, &self.options())
            .await
            .unwrap_or_else(|e| panic!("{statement}: {e}"));
        let names = stream.columns.iter().map(|c| c.name.clone()).collect();
        let mut rows = Vec::new();
        while let Some(batch) = stream.batches.next().await {
            rows.extend(batch.expect("batch").rows);
        }
        (names, rows, stream.affected)
    }

    async fn cleanup(self) {
        let statement = format!("db.getCollection(\"{}\").drop()", self.object.name);
        let _ = self.conn.execute(&statement, &self.options()).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn browses_with_and_without_seek() {
    let Some(fx) = Fixture::new("paging").await else {
        return;
    };
    let name = &fx.object.name;
    let docs: Vec<String> = (0..5000)
        .map(|i| {
            let extra = if i % 7 == 0 { ", note: 'x'" } else { "" };
            format!("{{n: {i}, group: {}{extra}}}", i % 13)
        })
        .collect();
    for chunk in docs.chunks(1000) {
        let statement = format!(
            "db.getCollection('{name}').insertMany([{}])",
            chunk.join(",")
        );
        let (_, _, affected) = fx.run(&statement).await;
        assert_eq!(affected, Some(chunk.len() as u64));
    }

    let mut by_offset = Vec::new();
    let mut by_seek = Vec::new();
    let mut last_key = None;
    let mut offset = 0;
    loop {
        let plain = BrowseRequest {
            offset,
            limit: 333,
            ..Default::default()
        };
        let page = fx.conn.browse(&fx.object, &plain).await.expect("page");
        assert_eq!(page.columns[0].name, "_id");
        assert_eq!(page.columns[0].kind, ValueKind::ObjectId);
        by_offset.extend(page.rows.iter().map(|r| r[0].clone()));

        let seeking = BrowseRequest {
            seek_after: last_key.clone(),
            ..plain
        };
        let page = fx
            .conn
            .browse(&fx.object, &seeking)
            .await
            .expect("seek page");
        by_seek.extend(page.rows.iter().map(|r| r[0].clone()));
        last_key = page.last_key.clone();
        assert!(last_key.is_some());
        offset += page.rows.len() as u64;
        if !page.more {
            break;
        }
    }
    assert_eq!(by_offset.len(), 5000);
    assert_eq!(by_offset, by_seek);

    // A sort by another field pages with skip, still in a total order.
    let mut sorted = Vec::new();
    let mut offset = 0;
    loop {
        let request = BrowseRequest {
            offset,
            limit: 1000,
            sort: vec![deltos_data::Sort {
                column: "group".into(),
                direction: deltos_data::SortDirection::Descending,
            }],
            ..Default::default()
        };
        let page = fx.conn.browse(&fx.object, &request).await.expect("sorted");
        assert!(page.last_key.is_none());
        sorted.extend(page.rows.iter().map(|r| r[0].clone()));
        offset += page.rows.len() as u64;
        if !page.more {
            break;
        }
    }
    let mut unique = sorted.clone();
    unique.sort_by_key(|v| v.to_string());
    unique.dedup();
    assert_eq!(unique.len(), 5000);

    let filtered = BrowseRequest {
        limit: 10_000,
        filters: vec![Filter {
            column: "group".into(),
            op: FilterOp::Eq,
            value: Value::text("3"),
            values: Vec::new(),
        }],
        raw_where: Some("{n: {$lt: 1000}}".into()),
        ..Default::default()
    };
    let page = fx.conn.browse(&fx.object, &filtered).await.expect("filter");
    let expected = (0..1000).filter(|i| i % 13 == 3).count();
    assert_eq!(page.rows.len(), expected);
    assert_eq!(
        fx.conn
            .count(&fx.object, &filtered, true)
            .await
            .expect("count"),
        expected as u64
    );
    let all = BrowseRequest::default();
    assert_eq!(
        fx.conn
            .count(&fx.object, &all, false)
            .await
            .expect("estimate"),
        5000
    );
    let columns: Vec<String> = page.columns.iter().map(|c| c.name.clone()).collect();
    assert!(columns.contains(&"note".to_string()), "{columns:?}");
    fx.cleanup().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn runs_shell_statements() {
    let Some(fx) = Fixture::new("shell").await else {
        return;
    };
    let name = &fx.object.name;
    let (_, _, affected) = fx
        .run(&format!(
            "db.getCollection('{name}').insertMany([{{_id: 1, a: 'x', when: ISODate('2024-01-02T03:04:05Z')}}, {{_id: 2, a: 'y', nested: {{b: [1, 2]}}}}])"
        ))
        .await;
    assert_eq!(affected, Some(2));
    let (names, rows, _) = fx
        .run(&format!(
            "db.getCollection('{name}').find({{}}).sort({{_id: -1}})"
        ))
        .await;
    // Columns are the fields of the first batch, in the order they were first seen.
    assert_eq!(names, ["_id", "a", "nested", "when"]);
    assert_eq!(rows[0][0], Value::Int(2));
    assert_eq!(rows[0][2], Value::Json(r#"{"b":[1,2]}"#.into()));
    let (_, rows, _) = fx
        .run(&format!("db.getCollection('{name}').find({{_id: 1}})"))
        .await;
    assert_eq!(rows[0][2], Value::text("2024-01-02T03:04:05Z"));
    let (names, rows, _) = fx
        .run(&format!(
            "db.getCollection('{name}').aggregate([{{$group: {{_id: null, total: {{$sum: '$_id'}}}}}}])"
        ))
        .await;
    assert_eq!(names, ["_id", "total"]);
    assert_eq!(rows[0][1], Value::Int(3));
    let (_, rows, _) = fx
        .run(&format!(
            "db.getCollection('{name}').countDocuments({{a: 'x'}})"
        ))
        .await;
    assert_eq!(rows[0][0], Value::Int(1));
    let (_, _, affected) = fx
        .run(&format!(
            "db.getCollection('{name}').updateMany({{}}, {{$set: {{flag: true}}}})"
        ))
        .await;
    assert_eq!(affected, Some(2));
    let (names, rows, _) = fx
        .run(&format!(
            "db.getCollection('{name}').find({{a: 'x'}}).explain('executionStats')"
        ))
        .await;
    assert_eq!(rows.len(), 1);
    assert!(names.contains(&"queryPlanner".to_string()), "{names:?}");
    assert!(names.contains(&"executionStats".to_string()), "{names:?}");
    let (_, rows, _) = fx.run("{buildInfo: 1}").await;
    assert_eq!(rows.len(), 1);
    let error = fx
        .conn
        .execute("db.x.find({a: })", &fx.options())
        .await
        .expect_err("syntax");
    assert!(error.to_string().contains("line 1"), "{error}");
    fx.cleanup().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn streams_and_cancels() {
    let Some(fx) = Fixture::new("stream").await else {
        return;
    };
    let name = &fx.object.name;
    let docs: Vec<String> = (0..2000).map(|i| format!("{{n: {i}}}")).collect();
    fx.run(&format!(
        "db.getCollection('{name}').insertMany([{}])",
        docs.join(",")
    ))
    .await;
    let options = ExecuteOptions {
        batch_size: 100,
        namespace: Some(fx.object.namespace.clone()),
    };
    let mut stream = fx
        .conn
        .execute(&format!("db.getCollection('{name}').find()"), &options)
        .await
        .expect("find");
    let first = stream.batches.next().await.expect("batch").expect("rows");
    assert_eq!(first.rows.len(), 100);
    let cancel = fx.conn.cancel_handle().expect("cancel");
    cancel.cancel().await.expect("cancel");
    let mut cancelled = false;
    while let Some(batch) = stream.batches.next().await {
        if let Err(Error::Cancelled) = batch {
            cancelled = true;
        }
    }
    assert!(cancelled);
    drop(stream);
    // The connection is usable after a cancel.
    let (_, rows, _) = fx
        .run(&format!("db.getCollection('{name}').find().limit(3)"))
        .await;
    assert_eq!(rows.len(), 3);
    fx.cleanup().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn describes_and_edits() {
    let Some(fx) = Fixture::new("edits").await else {
        return;
    };
    let name = &fx.object.name;
    fx.run(&format!(
        "db.getCollection('{name}').insertOne({{_id: ObjectId('65a1b2c3d4e5f60718293a4b'), n: NumberInt(1), when: ISODate('2024-01-01T00:00:00Z')}})"
    ))
    .await;
    let _ = fx
        .conn
        .execute(
            &format!("db.runCommand({{createIndexes: '{name}', indexes: [{{key: {{n: 1}}, name: 'n_1', unique: true}}]}})"),
            &fx.options(),
        )
        .await;
    let info = fx.conn.describe(&fx.object).await.expect("describe");
    assert_eq!(info.primary_key()[0].name, "_id");
    assert!(info.indexes.iter().any(|i| i.primary));
    assert!(
        info.definition
            .as_deref()
            .unwrap_or("")
            .contains("createCollection")
    );

    let key = vec![("_id".to_string(), Value::text("65a1b2c3d4e5f60718293a4b"))];
    let edits = [
        RowEdit::Update {
            key: key.clone(),
            changes: vec![
                ("n".to_string(), Value::text("5")),
                ("when".to_string(), Value::text("2025-06-01T00:00:00Z")),
            ],
        },
        RowEdit::Insert {
            values: vec![
                ("n".to_string(), Value::Int(9)),
                ("x".to_string(), Value::Null),
            ],
        },
    ];
    assert_eq!(
        fx.conn
            .apply_edits(&fx.object, &edits)
            .await
            .expect("edits"),
        2
    );
    let (_, rows, _) = fx
        .run(&format!(
            "db.getCollection('{name}').aggregate([{{$match: {{n: 5}}}}, {{$project: {{t: {{$type: '$n'}}, w: {{$type: '$when'}}}}}}])"
        ))
        .await;
    assert_eq!(rows[0][1], Value::text("int"));
    assert_eq!(rows[0][2], Value::text("date"));
    let missing = [RowEdit::Delete {
        key: vec![("_id".to_string(), Value::text("000000000000000000000000"))],
    }];
    assert!(fx.conn.apply_edits(&fx.object, &missing).await.is_err());
    let delete = [RowEdit::Delete { key }];
    assert_eq!(
        fx.conn
            .apply_edits(&fx.object, &delete)
            .await
            .expect("delete"),
        1
    );
    fx.cleanup().await;
}

#[test]
fn builds_urls_from_fields() {
    let config = ConnectionConfig {
        host: "db.example.com".into(),
        user: "app user".into(),
        password: Some("p@ss:word".into()),
        database: "shop".into(),
        options: vec![("authSource".into(), "admin".into())],
        ..Default::default()
    };
    assert_eq!(
        url_from_fields(&config),
        "mongodb://app%20user:p%40ss%3Aword@db.example.com:27017/shop?authSource=admin"
    );
    let srv = ConnectionConfig {
        host: "cluster0.example.net".into(),
        options: vec![("srv".into(), "true".into())],
        ..Default::default()
    };
    assert_eq!(url_from_fields(&srv), "mongodb+srv://cluster0.example.net/");
}

#[test]
fn fills_options_from_the_form() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let config = ConnectionConfig {
        url: Some("mongodb://localhost:27017/?tls=false".into()),
        user: "me".into(),
        password: Some("secret".into()),
        database: "shop".into(),
        tls: TlsMode::Require,
        ..Default::default()
    };
    let options = rt.block_on(client_options(&config)).expect("options");
    let credential = options.credential.expect("credential");
    assert_eq!(credential.username.as_deref(), Some("me"));
    assert_eq!(credential.password.as_deref(), Some("secret"));
    assert_eq!(options.default_database.as_deref(), Some("shop"));
    // The URL's own TLS choice wins over the form's.
    assert!(matches!(options.tls, Some(Tls::Disabled)));
    let bad = ConnectionConfig {
        url: Some("mongodb://".into()),
        ..Default::default()
    };
    let error = rt.block_on(client_options(&bad)).expect_err("bad url");
    assert!(error.to_string().contains("not valid"), "{error}");
}

#[test]
fn writes_definitions() {
    let spec = doc! { "name": "v", "type": "view", "options": { "viewOn": "orders", "pipeline": [{ "$match": { "a": 1 } }] } };
    let text = definition("v", Some(&spec), &[]);
    assert!(
        text.starts_with("db.createView(\"v\", \"orders\", ["),
        "{text}"
    );
    let spec = doc! { "name": "c", "options": { "capped": true, "size": 1024 } };
    let indexes = [
        doc! { "v": 2, "key": { "_id": 1 }, "name": "_id_" },
        doc! { "v": 2, "key": { "email": 1 }, "name": "email_1", "unique": true },
    ];
    let text = definition("c", Some(&spec), &indexes);
    assert!(text.contains("\"capped\": true"), "{text}");
    assert!(
        text.contains(r#"createIndex({"email":1}, {"name":"email_1","unique":true})"#),
        "{text}"
    );
    let index = index_of(&indexes[1]).expect("index");
    assert!(index.unique && !index.primary);
    assert_eq!(index.columns, ["email"]);
}

#[test]
fn maps_errors() {
    let error = mongo_error(mongodb::error::Error::custom("boom"));
    assert!(matches!(error, Error::Other(_)));
}

/// Every filter bar operator against a live server.
#[tokio::test(flavor = "multi_thread")]
async fn filter_operators_keep_the_right_documents() {
    let Some(fx) = Fixture::new("operators").await else {
        return;
    };
    let name = &fx.object.name;
    fx.run(&format!(
        "db.getCollection('{name}').insertMany([
           {{k: 1, name: 'Ada', n: 10}}, {{k: 2, name: 'grace', n: 20}},
           {{k: 3, name: '50% off', n: 30}}, {{k: 4, name: '', n: 40}},
           {{k: 5, name: null}}, {{k: 6, name: 'Linus_t', n: 50}}, {{k: 7, name: 'adam', n: 60}}])"
    ))
    .await;
    // A first page teaches the driver the fields' types.
    let _ = fx
        .conn
        .browse(
            &fx.object,
            &BrowseRequest {
                limit: 10,
                ..Default::default()
            },
        )
        .await
        .expect("browse");
    let ks = async |filters: Vec<Filter>, match_any: bool| -> Vec<i64> {
        let request = BrowseRequest {
            limit: 100,
            filters,
            match_any,
            sort: vec![deltos_data::Sort {
                column: "k".into(),
                direction: deltos_data::SortDirection::Ascending,
            }],
            ..Default::default()
        };
        let page = fx.conn.browse(&fx.object, &request).await.expect("browse");
        let n = fx
            .conn
            .count(&fx.object, &request, true)
            .await
            .expect("count");
        assert_eq!(n, page.rows.len() as u64);
        let k = page.columns.iter().position(|c| c.name == "k").expect("k");
        page.rows
            .iter()
            .map(|r| match r[k] {
                Value::Int(i) => i,
                _ => -1,
            })
            .collect()
    };
    let name = |op, text: &str| vec![Filter::new("name", op, Value::text(text))];
    assert_eq!(
        ks(name(FilterOp::NotContains, "ad"), false).await,
        [2, 3, 4, 5, 6]
    );
    assert_eq!(ks(name(FilterOp::EndsWith, "_T"), false).await, [6]);
    assert_eq!(ks(name(FilterOp::Like, "ad%"), false).await, [7]);
    assert_eq!(ks(name(FilterOp::IsEmpty, ""), false).await, [4, 5]);
    let n = |op, values: &[&str]| {
        vec![Filter::list(
            "n",
            op,
            values.iter().map(|v| Value::text(*v)).collect(),
        )]
    };
    assert_eq!(ks(n(FilterOp::In, &["20", "60"]), false).await, [2, 7]);
    assert_eq!(
        ks(n(FilterOp::Between, &["20", "40"]), false).await,
        [2, 3, 4]
    );
    let any = vec![
        Filter::new("n", FilterOp::Lt, Value::Int(15)),
        Filter::new("name", FilterOp::StartsWith, Value::text("lin")),
    ];
    assert_eq!(ks(any, true).await, [1, 6]);
    fx.cleanup().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn reads_replaces_and_inserts_whole_documents() {
    let Some(fx) = Fixture::new("documents").await else {
        return;
    };
    let name = &fx.object.name;
    fx.run(&format!(
        "db.getCollection('{name}').insertOne({{_id: ObjectId('65a1b2c3d4e5f60718293a4b'), n: NumberLong(7), f: 1.0, tags: ['a'], address: {{city: 'Oslo'}}}})"
    ))
    .await;
    let key = vec![("_id".to_string(), Value::text("65a1b2c3d4e5f60718293a4b"))];
    let text = fx
        .conn
        .document(&fx.object, &key)
        .await
        .expect("document")
        .expect("found");
    assert!(
        text.contains("ObjectId(\"65a1b2c3d4e5f60718293a4b\")"),
        "{text}"
    );
    assert!(text.contains("NumberLong(\"7\")"), "{text}");
    assert!(text.contains("f: 1.0"), "{text}");

    // Without _id the document keeps its own; with another _id it is refused.
    fx.conn
        .replace_document(
            &fx.object,
            &key,
            "{n: 8, address: {city: 'Bergen', zip: '5003'}}",
        )
        .await
        .expect("replace");
    let text = fx.conn.document(&fx.object, &key).await.unwrap().unwrap();
    assert!(text.contains("Bergen") && !text.contains("tags"), "{text}");
    assert!(
        text.trim_start().starts_with("{\n  _id: ObjectId"),
        "{text}"
    );
    let moved = fx
        .conn
        .replace_document(&fx.object, &key, "{_id: 5, n: 1}")
        .await;
    assert!(moved.is_err());
    let broken = fx.conn.replace_document(&fx.object, &key, "{n: ").await;
    assert!(broken.is_err_and(|e| e.to_string().contains("line 1")));

    let new_key = fx
        .conn
        .insert_document(
            &fx.object,
            "{_id: 'k2', items: [{sku: 'p', qty: 1}, {sku: 'q'}]}",
        )
        .await
        .expect("insert");
    assert_eq!(new_key, vec![("_id".to_string(), Value::text("k2"))]);
    assert!(
        fx.conn
            .document(&fx.object, &new_key)
            .await
            .unwrap()
            .is_some()
    );
    let gone = vec![("_id".to_string(), Value::text("nope"))];
    assert_eq!(fx.conn.document(&fx.object, &gone).await.unwrap(), None);

    let schema = fx.conn.schema(&fx.object, 100).await.expect("schema");
    assert_eq!(schema.sampled, 2);
    assert!(!schema.declared);
    assert!(schema.fields[0].key);
    assert_eq!(schema.field("address.city").map(|f| f.present), Some(1));
    let qty = schema.field("items.qty").expect("items.qty");
    assert_eq!((qty.present, qty.of), (1, 2));
    fx.cleanup().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn manages_indexes_and_collections() {
    let Some(fx) = Fixture::new("indexes").await else {
        return;
    };
    let name = &fx.object.name;
    fx.run(&format!("db.createCollection('{name}')")).await;
    let (_, _, _) = fx
        .run(&format!(
            "db.getCollection('{name}').createIndex({{status: 1, created: -1}}, {{unique: true}})"
        ))
        .await;
    let (columns, rows, _) = fx
        .run(&format!("db.getCollection('{name}').getIndexes()"))
        .await;
    let at = columns
        .iter()
        .position(|c| c == "name")
        .expect("name column");
    let names: Vec<_> = rows.iter().map(|r| r[at].clone()).collect();
    assert!(
        names.contains(&Value::text("status_1_created_-1")),
        "{names:?}"
    );
    fx.run(&format!(
        "db.getCollection('{name}').dropIndex('status_1_created_-1')"
    ))
    .await;
    let (_, rows, _) = fx
        .run(&format!("db.getCollection('{name}').getIndexes()"))
        .await;
    assert_eq!(rows.len(), 1);

    fx.run(&format!(
        "db.getCollection('{name}').insertOne({{_id: 1, n: 1}})"
    ))
    .await;
    let (columns, rows, _) = fx
        .run(&format!(
            "db.getCollection('{name}').findOneAndUpdate({{_id: 1}}, {{$inc: {{n: 1}}}}, {{returnDocument: 'after'}})"
        ))
        .await;
    let n = columns.iter().position(|c| c == "n").expect("n");
    assert_eq!(rows[0][n], Value::Int(2));
    let (_, rows, affected) = fx
        .run(&format!(
            "db.getCollection('{name}').findOneAndDelete({{_id: 99}})"
        ))
        .await;
    assert!(rows.is_empty());
    assert_eq!(affected, Some(0));

    let (columns, rows, _) = fx.run("show dbs").await;
    assert_eq!(columns.first().map(String::as_str), Some("name"));
    assert!(!rows.is_empty());
    let (_, rows, _) = fx.run(&format!("db.getCollection('{name}').stats()")).await;
    assert_eq!(rows.len(), 1);

    let renamed = format!("{name}_renamed");
    fx.run(&format!(
        "db.getCollection('{name}').renameCollection('{renamed}')"
    ))
    .await;
    let objects = fx
        .conn
        .objects(&fx.object.namespace)
        .await
        .expect("objects");
    assert!(objects.iter().any(|o| o.object.name == renamed));
    assert!(!objects.iter().any(|o| o.object.name == *name));
    fx.run(&format!("db.getCollection('{renamed}').drop()"))
        .await;
    fx.cleanup().await;
}

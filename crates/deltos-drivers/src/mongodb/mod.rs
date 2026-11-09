//! MongoDB, through the official Rust driver with rustls.
//!
//! A collection is shown as a table: one row per document, a column per top-level field
//! with `_id` first, in the order fields were first seen. Nested documents and arrays are
//! relaxed extended JSON in [`Value::Json`]; an ObjectId is its hex text with
//! [`ValueKind::ObjectId`](deltos_data::ValueKind::ObjectId), a date RFC 3339 text, a
//! decimal its exact text. The columns of a collection are remembered across pages, so
//! they only grow while scrolling.
//!
//! Browsing orders by the sort fields, then `_id`. When the order is `_id` alone, the
//! default, the next page seeks past the last `_id` (`$gt`, or a type MongoDB sorts
//! later, since it ranks types before values), so reading deep into a large collection
//! is as fast as reading its start. Sorting by another field pages with `skip`: such a
//! field may hold arrays, which MongoDB sorts by their smallest element but compares
//! element by element in a query, so no seek condition would match the sort exactly.
//! `last_key` is `None` then.
//!
//! Statements are written in the shell's language and parsed by [`shell`]. `find` and
//! `aggregate` stream: a task reads the server cursor one batch at a time and waits for
//! the caller before it asks for the next, so a result of any size takes the memory of
//! one batch. The columns of a streamed result are the fields of its first batch.
//!
//! Every `find` and `aggregate` carries a `comment` naming the operation. Cancelling stops
//! the local work at once and then finds the server operations by that comment and stops
//! them with `killOp`, so a long aggregation does not keep running on the server.
//!
//! Edits are applied document by document, each located by `_id` and checked to match
//! exactly one document. MongoDB has multi-document transactions only on replica sets and
//! sharded clusters, so edits do not rely on them; an error says how many edits were
//! saved before it.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bson::oid::ObjectId;
use bson::{Bson, Document, doc};
use deltos_data::{
    BrowsePage, BrowseRequest, CancelHandle, Capabilities, Column, Connection, ConnectionConfig,
    Driver, DriverKind, Error, ExecuteOptions, Index, Namespace, NamespaceKind, ObjectEntry,
    ObjectInfo, ObjectKind, ObjectRef, Result, Row, RowBatch, RowEdit, RowStream, TlsMode, Value,
};
use futures::future::BoxFuture;
use futures::{StreamExt, TryStreamExt};
use mongodb::options::{ClientOptions, Credential, Tls, TlsOptions};
use mongodb::{Client, Collection, Cursor, Database};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use tokio::runtime::Handle;
use tokio::sync::{mpsc, watch};

use crate::{runtime, util};

mod convert;
mod query;
mod shape;
pub mod shell;
#[cfg(test)]
mod tests;

use convert::Target;
use shape::Shape;
use shell::{Command, Operation};

/// Collections whose field lists are remembered between pages, so columns stay put while
/// scrolling. Past this many the memory is cleared and rebuilt as pages are read.
const MAX_SHAPES: usize = 256;

/// Opens MongoDB connections.
#[derive(Debug, Clone, Copy, Default)]
pub struct MongoDriver;

impl Driver for MongoDriver {
    fn kind(&self) -> DriverKind {
        DriverKind::Mongodb
    }

    fn connect<'a>(
        &'a self,
        config: &'a ConnectionConfig,
    ) -> BoxFuture<'a, Result<Arc<dyn Connection>>> {
        let config = config.clone();
        Box::pin(async move {
            let rt = runtime::handle()?;
            let connection = runtime::spawn(&rt.clone(), async move {
                MongoConnection::open(&config, rt).await
            })
            .await?;
            Ok(Arc::new(connection) as Arc<dyn Connection>)
        })
    }
}

/// An open MongoDB deployment: a standalone server, a replica set or a sharded cluster.
pub struct MongoConnection {
    inner: Arc<Inner>,
}

struct Inner {
    rt: Handle,
    client: Client,
    /// The database named in the URL or the form, used when a statement names none.
    default_db: Option<String>,
    version: String,
    shapes: Mutex<HashMap<(String, String), Shape>>,
    /// Bumped by a cancel; every running operation watches it.
    cancel: watch::Sender<u64>,
    /// The comment tag of each running operation, by operation number.
    running: Mutex<HashMap<u64, String>>,
    next_op: AtomicU64,
}

impl MongoConnection {
    async fn open(config: &ConnectionConfig, rt: Handle) -> Result<Self> {
        let options = client_options(config).await?;
        let default_db = options.default_database.clone();
        let client = Client::with_options(options).map_err(connect_error)?;
        let admin = client.database("admin");
        admin
            .run_command(doc! { "ping": 1 })
            .await
            .map_err(connect_error)?;
        let version = match admin.run_command(doc! { "buildInfo": 1 }).await {
            Ok(info) => match info.get_str("version") {
                Ok(v) => format!("MongoDB {v}"),
                Err(_) => "MongoDB".to_string(),
            },
            Err(_) => "MongoDB".to_string(),
        };
        let (cancel, _) = watch::channel(0);
        Ok(Self {
            inner: Arc::new(Inner {
                rt,
                client,
                default_db,
                version,
                shapes: Mutex::new(HashMap::new()),
                cancel,
                running: Mutex::new(HashMap::new()),
                next_op: AtomicU64::new(1),
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

/// One running operation: its tag on the server, and a way to hear about a cancel. It
/// leaves the running list when dropped, which for a stream is when the stream ends or
/// is dropped.
struct Op {
    inner: Arc<Inner>,
    id: u64,
    tag: String,
    cancelled: watch::Receiver<u64>,
}

impl Op {
    fn start(inner: &Arc<Inner>) -> Self {
        let id = inner.next_op.fetch_add(1, Ordering::Relaxed);
        let tag = format!("deltos:{}", ObjectId::new().to_hex());
        if let Ok(mut running) = inner.running.lock() {
            running.insert(id, tag.clone());
        }
        Op {
            inner: inner.clone(),
            id,
            tag,
            cancelled: inner.cancel.subscribe(),
        }
    }

    /// Runs `future` unless a cancel comes first.
    async fn run<T>(&mut self, future: impl std::future::Future<Output = Result<T>>) -> Result<T> {
        tokio::select! {
            result = future => result,
            _ = self.cancelled.changed() => Err(Error::Cancelled),
        }
    }
}

impl Drop for Op {
    fn drop(&mut self) {
        if let Ok(mut running) = self.inner.running.lock() {
            running.remove(&self.id);
        }
    }
}

impl Inner {
    fn database(&self, namespace: &[String]) -> Result<Database> {
        let name = namespace
            .first()
            .map(String::as_str)
            .or(self.default_db.as_deref())
            .ok_or_else(|| Error::Query("Choose a database first.".to_string()))?;
        Ok(self.client.database(name))
    }

    fn collection(&self, object: &ObjectRef) -> Result<Collection<Document>> {
        Ok(self.database(&object.namespace)?.collection(&object.name))
    }

    fn shape_key(&self, object: &ObjectRef) -> (String, String) {
        let db = object
            .namespace
            .first()
            .cloned()
            .or_else(|| self.default_db.clone())
            .unwrap_or_default();
        (db, object.name.clone())
    }

    /// Runs `f` on the remembered shape of a collection, creating it when needed.
    fn with_shape<T>(&self, object: &ObjectRef, f: impl FnOnce(&mut Shape) -> T) -> T {
        let key = self.shape_key(object);
        match self.shapes.lock() {
            Ok(mut shapes) => {
                if shapes.len() >= MAX_SHAPES && !shapes.contains_key(&key) {
                    shapes.clear();
                }
                f(shapes.entry(key).or_insert_with(Shape::keyed))
            }
            Err(_) => f(&mut Shape::keyed()),
        }
    }

    async fn namespaces(&self) -> Result<Vec<Namespace>> {
        let mut names = match self
            .client
            .list_database_names()
            .authorized_databases(true)
            .await
        {
            Ok(names) => names,
            // Users limited to one database may not list the others.
            Err(e) => match &self.default_db {
                Some(db) => vec![db.clone()],
                None => return Err(mongo_error(e)),
            },
        };
        if let Some(db) = &self.default_db
            && !names.contains(db)
        {
            names.push(db.clone());
        }
        names.sort();
        Ok(names
            .into_iter()
            .map(|name| Namespace {
                path: vec![name],
                kind: NamespaceKind::Database,
            })
            .collect())
    }

    async fn objects(&self, namespace: &[String]) -> Result<Vec<ObjectEntry>> {
        let db = self.database(namespace)?;
        let db_name = db.name().to_string();
        let mut specs = list_collections(&db, doc! {}, true).await?;
        specs.retain(|spec| {
            spec.get_str("name")
                .is_ok_and(|name| !name.starts_with("system."))
        });
        let mut entries: Vec<(ObjectRef, bool)> = specs
            .iter()
            .filter_map(|spec| {
                let name = spec.get_str("name").ok()?;
                let view = spec.get_str("type").ok() == Some("view");
                let kind = if view {
                    ObjectKind::View
                } else {
                    ObjectKind::Collection
                };
                Some((
                    ObjectRef {
                        namespace: vec![db_name.clone()],
                        name: name.to_string(),
                        kind,
                    },
                    view,
                ))
            })
            .collect();
        entries.sort_by(|a, b| a.0.name.cmp(&b.0.name));
        // Statistics cost a round trip per collection; past this many the sidebar would
        // wait too long for numbers it can do without.
        const STATS_LIMIT: usize = 500;
        let with_stats = entries.len() <= STATS_LIMIT;
        let out = futures::stream::iter(entries)
            .map(|(object, view)| {
                let db = &db;
                async move {
                    let stats = if with_stats && !view {
                        collection_stats(db, &object.name).await
                    } else {
                        Stats::default()
                    };
                    ObjectEntry {
                        object,
                        estimated_rows: stats.count,
                        size_bytes: stats.size,
                        comment: None,
                    }
                }
            })
            .buffered(8)
            .collect()
            .await;
        Ok(out)
    }

    async fn describe(&self, object: &ObjectRef) -> Result<ObjectInfo> {
        let db = self.database(&object.namespace)?;
        let collection: Collection<Document> = db.collection(&object.name);
        let mut cursor = collection
            .find(doc! {})
            .limit(100)
            .batch_size(100)
            .await
            .map_err(mongo_error)?;
        let mut sample = Shape::keyed();
        while let Some(doc) = cursor.try_next().await.map_err(mongo_error)? {
            sample.add(&doc);
        }
        drop(cursor);
        let columns = sample.columns();
        self.with_shape(object, |shape| {
            if shape.len() <= 1 {
                *shape = sample;
            }
        });

        let spec = list_collections(&db, doc! { "name": &object.name }, false)
            .await?
            .into_iter()
            .next();
        let view = spec
            .as_ref()
            .is_some_and(|s| s.get_str("type").ok() == Some("view"));
        let index_docs = if view {
            Vec::new()
        } else {
            list_indexes(&db, &object.name).await.unwrap_or_default()
        };
        let indexes = index_docs.iter().filter_map(index_of).collect();
        let stats = if view {
            Stats::default()
        } else {
            collection_stats(&db, &object.name).await
        };
        Ok(ObjectInfo {
            columns,
            indexes,
            foreign_keys: Vec::new(),
            estimated_rows: stats.count,
            size_bytes: stats.size,
            comment: None,
            definition: Some(definition(&object.name, spec.as_ref(), &index_docs)),
        })
    }

    async fn browse(
        &self,
        object: &ObjectRef,
        request: &BrowseRequest,
        tag: Bson,
    ) -> Result<BrowsePage> {
        let collection = self.collection(object)?;
        let kinds = self.with_shape(object, |shape| {
            request
                .filters
                .iter()
                .filter_map(|f| Some((f.column.clone(), shape.kind(&f.column)?)))
                .collect::<HashMap<_, _>>()
        });
        let mut conditions = query::filter(request, |name| kinds.get(name).copied())?;
        let (sort, seek_order) = query::sort(request);
        let seek = match (seek_order, request.seek_after.as_deref()) {
            (Some(descending), Some(after)) if request.offset > 0 => {
                query::seek_value(after).map(|last| query::seek(&last, descending))
            }
            _ => None,
        };
        let skip = if seek.is_some() { 0 } else { request.offset };
        if let Some(seek) = seek {
            conditions.push(seek);
        }
        let filter = query::all_of(conditions);
        let limit = util::batch_size(request.limit.max(1)) as i64;

        let open = |disk: bool| {
            let mut find = collection
                .find(filter.clone())
                .sort(sort.clone())
                .comment(tag.clone())
                .limit(limit + 1)
                .batch_size((limit + 1).min(i64::from(u32::MAX)) as u32);
            if skip > 0 {
                find = find.skip(skip);
            }
            if disk {
                find = find.allow_disk_use(true);
            }
            find
        };
        // Sorting by a field without an index can pass the server's in-memory sort limit;
        // allowing it to spill to disk keeps such pages working. Servers before 4.4 do not
        // know the option.
        let mut cursor = match open(true).await {
            Ok(cursor) => cursor,
            Err(e) if e.to_string().contains("allowDiskUse") => {
                open(false).await.map_err(mongo_error)?
            }
            Err(e) => return Err(mongo_error(e)),
        };
        let mut docs = Vec::with_capacity(limit as usize + 1);
        while (docs.len() as i64) <= limit {
            match cursor.try_next().await.map_err(mongo_error)? {
                Some(doc) => docs.push(doc),
                None => break,
            }
        }
        drop(cursor);
        let more = docs.len() as i64 > limit;
        docs.truncate(limit as usize);
        let last_key = match (seek_order, docs.last().and_then(|d| d.get("_id"))) {
            (Some(_), Some(id)) => Some(vec![(
                "_id".to_string(),
                Value::Json(convert::canonical_text(id.clone()).into()),
            )]),
            _ => None,
        };
        let (columns, rows) = self.with_shape(object, |shape| {
            for doc in &docs {
                shape.add(doc);
            }
            let columns = shape.columns();
            let rows: Vec<Row> = docs.into_iter().map(|doc| shape.row(doc)).collect();
            (columns, rows)
        });
        Ok(BrowsePage {
            columns: Arc::from(columns),
            rows,
            more,
            last_key,
        })
    }

    async fn count(
        &self,
        object: &ObjectRef,
        request: &BrowseRequest,
        exact: bool,
        tag: Bson,
    ) -> Result<u64> {
        let collection = self.collection(object)?;
        let kinds = self.with_shape(object, |shape| {
            request
                .filters
                .iter()
                .filter_map(|f| Some((f.column.clone(), shape.kind(&f.column)?)))
                .collect::<HashMap<_, _>>()
        });
        let filter = query::all_of(query::filter(request, |name| kinds.get(name).copied())?);
        if filter.is_empty() && !exact && object.kind != ObjectKind::View {
            // The estimate reads the collection's metadata and is instant at any size.
            if let Ok(n) = collection
                .estimated_document_count()
                .comment(tag.clone())
                .await
            {
                return Ok(n);
            }
        }
        collection
            .count_documents(filter)
            .comment(tag)
            .await
            .map_err(mongo_error)
    }

    async fn execute(
        self: Arc<Self>,
        statement: String,
        options: ExecuteOptions,
    ) -> Result<RowStream> {
        let command = shell::parse(&statement).map_err(|e| Error::Query(e.describe(&statement)))?;
        let batch = util::batch_size(options.batch_size) as u32;
        let namespace = options.namespace.unwrap_or_default();
        let mut op = Op::start(&self);
        let in_db = |database: Option<String>| -> Result<Database> {
            match database {
                Some(name) => Ok(self.client.database(&name)),
                None => self
                    .database(&namespace)
                    .or_else(|_| Ok(self.client.database("test"))),
            }
        };
        match command {
            Command::Run {
                database,
                admin,
                command,
            } => {
                let db = if admin {
                    self.client.database("admin")
                } else {
                    in_db(database)?
                };
                if query::returns_cursor(&command) {
                    let command = query::prepare_cursor_command(command, batch);
                    let cursor = op
                        .run(async {
                            db.run_cursor_command(command)
                                .batch_size(batch)
                                .await
                                .map_err(mongo_error)
                        })
                        .await?;
                    stream_cursor(&self.rt, cursor, batch, op).await
                } else {
                    let reply = op
                        .run(async { db.run_command(command).await.map_err(mongo_error) })
                        .await?;
                    Ok(documents_stream(vec![reply], batch))
                }
            }
            Command::ListCollections { database } => {
                let db = in_db(database)?;
                let specs = op.run(list_collections(&db, doc! {}, true)).await?;
                let docs = specs
                    .into_iter()
                    .map(|spec| {
                        doc! {
                            "name": spec.get_str("name").unwrap_or_default(),
                            "type": spec.get_str("type").unwrap_or("collection"),
                        }
                    })
                    .collect();
                Ok(documents_stream(docs, batch))
            }
            Command::Collection {
                database,
                collection,
                operation,
            } => {
                let db = in_db(database)?;
                let inner = self.clone();
                inner
                    .run_operation(db, collection, operation, batch, op)
                    .await
            }
        }
    }

    async fn run_operation(
        &self,
        db: Database,
        name: String,
        operation: Operation,
        batch: u32,
        mut op: Op,
    ) -> Result<RowStream> {
        let collection: Collection<Document> = db.collection(&name);
        let tag = Bson::String(op.tag.clone());
        match operation {
            Operation::Find(find) if find.count => {
                let mut count = collection.count_documents(find.filter).comment(tag);
                if let Some(skip) = find.skip {
                    count = count.skip(skip);
                }
                if let Some(limit) = find.limit.filter(|l| *l != 0) {
                    count = count.limit(limit.unsigned_abs());
                }
                let n = op.run(async { count.await.map_err(mongo_error) }).await?;
                Ok(count_stream(n))
            }
            Operation::Find(find) => {
                let command = query::find_command(&name, &find, batch, &op.tag);
                let cursor = op
                    .run(async {
                        db.run_cursor_command(command)
                            .batch_size(find.batch_size.unwrap_or(batch).max(1))
                            .comment(tag)
                            .await
                            .map_err(mongo_error)
                    })
                    .await?;
                stream_cursor(&self.rt, cursor, batch, op).await
            }
            Operation::Aggregate { pipeline, options } => {
                let command = query::aggregate_command(
                    Bson::String(name.clone()),
                    &pipeline,
                    &options,
                    batch,
                    &op.tag,
                );
                let cursor = op
                    .run(async {
                        db.run_cursor_command(command)
                            .batch_size(batch)
                            .comment(tag)
                            .await
                            .map_err(mongo_error)
                    })
                    .await?;
                stream_cursor(&self.rt, cursor, batch, op).await
            }
            Operation::Count { filter } => {
                let n = op
                    .run(async {
                        collection
                            .count_documents(filter)
                            .comment(tag)
                            .await
                            .map_err(mongo_error)
                    })
                    .await?;
                Ok(count_stream(n))
            }
            Operation::EstimatedCount => {
                let n = op
                    .run(async {
                        collection
                            .estimated_document_count()
                            .await
                            .map_err(mongo_error)
                    })
                    .await?;
                Ok(count_stream(n))
            }
            Operation::Distinct { field, filter } => {
                let values = op
                    .run(async {
                        collection
                            .distinct(&field, filter)
                            .comment(tag)
                            .await
                            .map_err(mongo_error)
                    })
                    .await?;
                let docs = values
                    .into_iter()
                    .map(|value| {
                        let mut doc = Document::new();
                        doc.insert(field.clone(), value);
                        doc
                    })
                    .collect();
                Ok(documents_stream(docs, batch))
            }
            Operation::InsertOne(document) => {
                let result = op
                    .run(async { collection.insert_one(document).await.map_err(mongo_error) })
                    .await?;
                let mut stream = RowStream::affected(Some(1));
                stream.messages.push(format!(
                    "Inserted a document with _id {}.",
                    convert::json_text(result.inserted_id)
                ));
                Ok(stream)
            }
            Operation::InsertMany(documents) => {
                if documents.is_empty() {
                    return Err(Error::Query(
                        "insertMany needs at least one document.".to_string(),
                    ));
                }
                let result = op
                    .run(async { collection.insert_many(documents).await.map_err(mongo_error) })
                    .await?;
                let n = result.inserted_ids.len() as u64;
                let mut stream = RowStream::affected(Some(n));
                stream.messages.push(format!(
                    "Inserted {n} document{}.",
                    if n == 1 { "" } else { "s" }
                ));
                Ok(stream)
            }
            Operation::Update {
                filter,
                update,
                many,
                upsert,
            } => {
                let update = match update {
                    shell::Update::Document(doc) => {
                        mongodb::options::UpdateModifications::Document(doc)
                    }
                    shell::Update::Pipeline(stages) => {
                        mongodb::options::UpdateModifications::Pipeline(stages)
                    }
                };
                let result = op
                    .run(async {
                        if many {
                            collection.update_many(filter, update).upsert(upsert).await
                        } else {
                            collection.update_one(filter, update).upsert(upsert).await
                        }
                        .map_err(mongo_error)
                    })
                    .await?;
                Ok(update_stream(
                    result.matched_count,
                    result.modified_count,
                    result.upserted_id,
                ))
            }
            Operation::Replace {
                filter,
                replacement,
                upsert,
            } => {
                let result = op
                    .run(async {
                        collection
                            .replace_one(filter, replacement)
                            .upsert(upsert)
                            .await
                            .map_err(mongo_error)
                    })
                    .await?;
                Ok(update_stream(
                    result.matched_count,
                    result.modified_count,
                    result.upserted_id,
                ))
            }
            Operation::Delete { filter, many } => {
                let result = op
                    .run(async {
                        if many {
                            collection.delete_many(filter).await
                        } else {
                            collection.delete_one(filter).await
                        }
                        .map_err(mongo_error)
                    })
                    .await?;
                Ok(RowStream::affected(Some(result.deleted_count)))
            }
            Operation::Drop => {
                op.run(async { collection.drop().await.map_err(mongo_error) })
                    .await?;
                let mut stream = RowStream::affected(None);
                stream.messages.push(format!("Dropped collection {name}."));
                if let Ok(mut shapes) = self.shapes.lock() {
                    shapes.remove(&(db.name().to_string(), name));
                }
                Ok(stream)
            }
        }
    }

    async fn apply_edits(&self, object: &ObjectRef, edits: &[RowEdit]) -> Result<u64> {
        let collection = self.collection(object)?;
        let mut saved = 0u64;
        for (n, edit) in edits.iter().enumerate() {
            let result = self.apply_edit(object, &collection, edit).await;
            match result {
                Ok(()) => saved += 1,
                Err(e) if edits.len() == 1 => return Err(e),
                Err(e) => {
                    return Err(Error::Query(format!(
                        "Edit {} of {} failed, after {saved} were saved: {e}",
                        n + 1,
                        edits.len()
                    )));
                }
            }
        }
        Ok(saved)
    }

    async fn apply_edit(
        &self,
        object: &ObjectRef,
        collection: &Collection<Document>,
        edit: &RowEdit,
    ) -> Result<()> {
        let kind_target = |name: &str| {
            self.with_shape(object, |shape| shape.kind(name))
                .map_or(Target::Any, Target::for_kind)
        };
        match edit {
            RowEdit::Update { key, changes } => {
                if changes.is_empty() {
                    return Ok(());
                }
                if changes.iter().any(|(name, _)| name == "_id") {
                    return Err(Error::Query(
                        "The _id of a document cannot be changed; insert a copy instead."
                            .to_string(),
                    ));
                }
                let current = locate(collection, key, &kind_target).await?;
                let id = current.get("_id").cloned().unwrap_or(Bson::Null);
                let mut set = Document::new();
                for (name, value) in changes {
                    let target = current
                        .get(name)
                        .map_or_else(|| kind_target(name), Target::of);
                    set.insert(name.clone(), convert::to_bson(value, target));
                }
                let result = collection
                    .update_one(doc! { "_id": id }, doc! { "$set": set })
                    .await
                    .map_err(mongo_error)?;
                if result.matched_count != 1 {
                    return Err(gone());
                }
                Ok(())
            }
            RowEdit::Insert { values } => {
                // Empty cells are left out rather than stored as null, so a document
                // added from a wide grid holds only the fields that were filled in.
                let mut document = Document::new();
                for (name, value) in values {
                    if value.is_null() {
                        continue;
                    }
                    document.insert(name.clone(), convert::to_bson(value, kind_target(name)));
                }
                collection.insert_one(document).await.map_err(mongo_error)?;
                Ok(())
            }
            RowEdit::Delete { key } => {
                let current = locate(collection, key, &kind_target).await?;
                let id = current.get("_id").cloned().unwrap_or(Bson::Null);
                let result = collection
                    .delete_one(doc! { "_id": id })
                    .await
                    .map_err(mongo_error)?;
                if result.deleted_count != 1 {
                    return Err(gone());
                }
                Ok(())
            }
        }
    }

    async fn cancel(&self) {
        let tags: Vec<String> = self
            .running
            .lock()
            .map(|running| running.values().cloned().collect())
            .unwrap_or_default();
        self.cancel
            .send_modify(|epoch| *epoch = epoch.wrapping_add(1));
        let admin = self.client.database("admin");
        for tag in tags {
            kill_tagged(&admin, &tag).await;
        }
    }
}

/// Stops the server operations carrying `tag`. Failing is not an error: the user may lack
/// the right to see or stop operations, and the local side has stopped already.
async fn kill_tagged(admin: &Database, tag: &str) {
    let pipeline = [
        doc! { "$currentOp": {} },
        doc! { "$match": { "$or": [
            { "command.comment": tag },
            { "cursor.originatingCommand.comment": tag },
            { "originatingCommand.comment": tag },
        ] } },
        doc! { "$project": { "opid": 1 } },
    ];
    let ops: Vec<Document> = match admin.aggregate(pipeline).await {
        Ok(cursor) => cursor.try_collect().await.unwrap_or_default(),
        Err(e) => {
            tracing::debug!("could not list operations to cancel: {e}");
            return;
        }
    };
    for op in ops {
        if let Some(opid) = op.get("opid")
            && let Err(e) = admin
                .run_command(doc! { "killOp": 1, "op": opid.clone() })
                .await
        {
            tracing::debug!("could not stop operation: {e}");
        }
    }
}

/// Finds the one document a key names. `_id` text is tried as an ObjectId, a UUID and a
/// string, since the grid shows them alike.
async fn locate(
    collection: &Collection<Document>,
    key: &[(String, Value)],
    kind_target: &impl Fn(&str) -> Target,
) -> Result<Document> {
    if key.is_empty() {
        return Err(Error::Query(
            "This document cannot be edited because it has no _id.".to_string(),
        ));
    }
    let mut filter = Document::new();
    for (name, value) in key {
        if name == "_id" {
            filter.insert("_id", doc! { "$in": convert::key_candidates(value) });
        } else {
            filter.insert(name.clone(), convert::to_bson(value, kind_target(name)));
        }
    }
    let found: Vec<Document> = collection
        .find(filter)
        .limit(2)
        .await
        .map_err(mongo_error)?
        .try_collect()
        .await
        .map_err(mongo_error)?;
    match found.len() {
        1 => found.into_iter().next().ok_or_else(gone),
        0 => Err(gone()),
        _ => Err(Error::Query(
            "The key matches more than one document, so nothing was changed.".to_string(),
        )),
    }
}

fn gone() -> Error {
    Error::Query(
        "The document was not found; it may have been changed or deleted since it was read."
            .to_string(),
    )
}

/// Reads a cursor's first batch for the columns, then streams the rest from a task that
/// reads the next batch only once the caller has taken the last.
async fn stream_cursor(
    rt: &Handle,
    mut cursor: Cursor<Document>,
    batch: u32,
    mut op: Op,
) -> Result<RowStream> {
    let batch = batch.max(1) as usize;
    let mut first = Vec::with_capacity(batch.min(1024));
    let mut ended = false;
    while first.len() < batch {
        let next = op
            .run(async { cursor.try_next().await.map_err(mongo_error) })
            .await?;
        match next {
            Some(doc) => first.push(doc),
            None => {
                ended = true;
                break;
            }
        }
    }
    let shape = Shape::for_documents(&first);
    let columns: Arc<[Column]> = Arc::from(shape.columns());
    let first: Vec<Row> = first.into_iter().map(|doc| shape.row(doc)).collect();
    let (sender, receiver) = mpsc::channel(1);
    rt.spawn(async move {
        if !first.is_empty() && sender.send(Ok(RowBatch { rows: first })).await.is_err() {
            return;
        }
        if ended {
            return;
        }
        loop {
            let permit = tokio::select! {
                permit = sender.reserve() => match permit {
                    Ok(permit) => permit,
                    // The stream was dropped; dropping the cursor closes it on the server.
                    Err(_) => return,
                },
                _ = op.cancelled.changed() => {
                    let _ = sender.send(Err(Error::Cancelled)).await;
                    return;
                }
            };
            let mut rows = Vec::with_capacity(batch.min(1024));
            let mut done = false;
            while rows.len() < batch {
                let next = tokio::select! {
                    next = cursor.try_next() => next,
                    _ = op.cancelled.changed() => {
                        permit.send(Err(Error::Cancelled));
                        return;
                    }
                };
                match next {
                    Ok(Some(doc)) => rows.push(shape.row(doc)),
                    Ok(None) => {
                        done = true;
                        break;
                    }
                    Err(e) => {
                        permit.send(Err(mongo_error(e)));
                        return;
                    }
                }
            }
            if !rows.is_empty() {
                permit.send(Ok(RowBatch { rows }));
            }
            if done {
                return;
            }
        }
    });
    Ok(RowStream {
        columns,
        batches: util::channel_stream(receiver),
        affected: None,
        messages: Vec::new(),
    })
}

/// A result already in memory (a command reply, distinct values), in batches.
fn documents_stream(docs: Vec<Document>, batch: u32) -> RowStream {
    let shape = Shape::for_documents(&docs);
    let columns: Arc<[Column]> = Arc::from(shape.columns());
    let batch = batch.max(1) as usize;
    let mut batches = Vec::new();
    let mut rows = Vec::with_capacity(batch.min(docs.len()));
    for doc in docs {
        rows.push(shape.row(doc));
        if rows.len() == batch {
            batches.push(Ok(RowBatch {
                rows: std::mem::take(&mut rows),
            }));
        }
    }
    if !rows.is_empty() {
        batches.push(Ok(RowBatch { rows }));
    }
    RowStream {
        columns,
        batches: Box::pin(futures::stream::iter(batches)),
        affected: None,
        messages: Vec::new(),
    }
}

fn count_stream(n: u64) -> RowStream {
    let n = i64::try_from(n).unwrap_or(i64::MAX);
    documents_stream(vec![doc! { "count": n }], 1)
}

fn update_stream(matched: u64, modified: u64, upserted: Option<Bson>) -> RowStream {
    let mut stream = RowStream::affected(Some(modified));
    stream
        .messages
        .push(format!("Matched {matched}, modified {modified}."));
    if let Some(id) = upserted {
        stream.messages.push(format!(
            "Inserted a document with _id {}.",
            convert::json_text(id)
        ));
    }
    stream
}

/// Collection specifications from `listCollections`. `name_only` skips each collection's
/// options and so takes no locks on the server.
async fn list_collections(
    db: &Database,
    filter: Document,
    name_only: bool,
) -> Result<Vec<Document>> {
    let mut command =
        doc! { "listCollections": 1, "filter": filter.clone(), "nameOnly": name_only };
    if name_only {
        // Lets users who may read only some collections list those.
        command.insert("authorizedCollections", true);
    }
    let cursor = match db.run_cursor_command(command).await {
        Ok(cursor) => cursor,
        Err(_) if name_only => db
            .run_cursor_command(doc! { "listCollections": 1, "filter": filter, "nameOnly": true })
            .await
            .map_err(mongo_error)?,
        Err(e) => return Err(mongo_error(e)),
    };
    cursor.try_collect().await.map_err(mongo_error)
}

async fn list_indexes(db: &Database, collection: &str) -> Result<Vec<Document>> {
    let cursor = db
        .run_cursor_command(doc! { "listIndexes": collection })
        .await
        .map_err(mongo_error)?;
    cursor.try_collect().await.map_err(mongo_error)
}

fn index_of(spec: &Document) -> Option<Index> {
    let name = spec.get_str("name").ok()?.to_string();
    let key = spec.get_document("key").ok()?;
    let columns = key.keys().cloned().collect();
    let method = key.values().find_map(|v| match v {
        Bson::String(kind) => Some(kind.clone()),
        _ => None,
    });
    let primary = name == "_id_";
    Some(Index {
        unique: primary || spec.get_bool("unique").unwrap_or(false),
        name,
        columns,
        primary,
        method,
    })
}

#[derive(Debug, Default)]
struct Stats {
    count: Option<u64>,
    size: Option<u64>,
}

/// The document count and on-disk size (data and indexes) of a collection, summed over
/// shards. Users without the right to read statistics get none.
async fn collection_stats(db: &Database, name: &str) -> Stats {
    let pipeline = [doc! { "$collStats": { "storageStats": {} } }];
    let collection: Collection<Document> = db.collection(name);
    let docs: Vec<Document> = match collection.aggregate(pipeline).await {
        Ok(cursor) => cursor.try_collect().await.unwrap_or_default(),
        Err(_) => return Stats::default(),
    };
    let mut stats = Stats::default();
    for doc in docs {
        let Ok(storage) = doc.get_document("storageStats") else {
            continue;
        };
        let number = |key: &str| storage.get(key).and_then(as_u64);
        if let Some(count) = number("count") {
            stats.count = Some(stats.count.unwrap_or(0) + count);
        }
        let size = number("totalSize").or_else(|| {
            Some(number("storageSize").unwrap_or(0) + number("totalIndexSize").unwrap_or(0))
        });
        if let Some(size) = size {
            stats.size = Some(stats.size.unwrap_or(0) + size);
        }
    }
    stats
}

fn as_u64(value: &Bson) -> Option<u64> {
    match value {
        Bson::Int32(i) => u64::try_from(*i).ok(),
        Bson::Int64(i) => u64::try_from(*i).ok(),
        Bson::Double(f) if *f >= 0.0 && f.is_finite() => Some(*f as u64),
        _ => None,
    }
}

/// Shell statements that recreate a collection or view with its options and indexes.
fn definition(name: &str, spec: Option<&Document>, indexes: &[Document]) -> String {
    let quoted = serde_json::to_string(name).unwrap_or_else(|_| format!("\"{name}\""));
    let mut options = spec
        .and_then(|s| s.get_document("options").ok())
        .cloned()
        .unwrap_or_default();
    let mut out = String::new();
    let view_on = options.remove("viewOn");
    match view_on {
        Some(Bson::String(source)) => {
            let pipeline = options
                .remove("pipeline")
                .unwrap_or(Bson::Array(Vec::new()));
            let source = serde_json::to_string(&source).unwrap_or_default();
            out.push_str(&format!(
                "db.createView({quoted}, {source}, {}",
                convert::pretty_json(pipeline)
            ));
            if !options.is_empty() {
                out.push_str(&format!(
                    ", {}",
                    convert::pretty_json(Bson::Document(options))
                ));
            }
            out.push_str(");\n");
            return out;
        }
        _ => {
            out.push_str(&format!("db.createCollection({quoted}"));
            if !options.is_empty() {
                out.push_str(&format!(
                    ", {}",
                    convert::pretty_json(Bson::Document(options))
                ));
            }
            out.push_str(");\n");
        }
    }
    for index in indexes {
        if index.get_str("name").ok() == Some("_id_") {
            continue;
        }
        let Ok(key) = index.get_document("key") else {
            continue;
        };
        let mut rest = index.clone();
        for field in ["key", "v", "ns"] {
            rest.remove(field);
        }
        out.push_str(&format!(
            "db.getCollection({quoted}).createIndex({}, {});\n",
            convert::json_text(Bson::Document(key.clone())),
            convert::json_text(Bson::Document(rest))
        ));
    }
    out
}

/// Characters a user name or password must escape in a URL.
const USERINFO: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// The client's options from the connection form or URL.
///
/// A URL is given to the driver's own parser, which knows every option; the form's
/// fields then fill in only what it leaves out. MongoDB has no "TLS if offered": a plain
/// `mongodb://` connection is plain unless the URL or the form asks for TLS, and
/// `mongodb+srv://` uses TLS unless told otherwise, so [`TlsMode::Prefer`] keeps the
/// URL's choice. Without a URL, the form builds one: option `srv=true` makes it a
/// `mongodb+srv://` address, and other options are passed as URL options
/// (`authSource`, `replicaSet`, `directConnection` and so on).
async fn client_options(config: &ConnectionConfig) -> Result<ClientOptions> {
    let url = match config
        .url
        .as_deref()
        .map(str::trim)
        .filter(|u| !u.is_empty())
    {
        Some(url) => url.to_string(),
        None => url_from_fields(config),
    };
    let mut options = ClientOptions::parse(&url)
        .await
        .map_err(|e| Error::Connect(format!("the connection URL is not valid: {}", brief(&e))))?;
    let user = config.user.trim();
    match options.credential.as_mut() {
        Some(credential) => {
            if credential.username.is_none() && !user.is_empty() {
                credential.username = Some(user.to_string());
            }
            if credential.password.is_none() {
                credential.password = config.password.clone();
            }
        }
        None if !user.is_empty() => {
            let mut credential = Credential::default();
            credential.username = Some(user.to_string());
            credential.password = config.password.clone();
            options.credential = Some(credential);
        }
        None => {}
    }
    if options.default_database.is_none() && !config.database.trim().is_empty() {
        options.default_database = Some(config.database.trim().to_string());
    }
    if options.tls.is_none() {
        options.tls = match config.tls {
            TlsMode::Disable => Some(Tls::Disabled),
            TlsMode::Prefer => None,
            TlsMode::Require => {
                let mut tls = TlsOptions::default();
                tls.allow_invalid_certificates = Some(true);
                Some(Tls::Enabled(tls))
            }
            TlsMode::VerifyFull => Some(Tls::Enabled(TlsOptions::default())),
        };
    }
    options.app_name.get_or_insert_with(|| "Deltos".to_string());
    options
        .connect_timeout
        .get_or_insert(Duration::from_secs(10));
    options
        .server_selection_timeout
        .get_or_insert(Duration::from_secs(10));
    Ok(options)
}

fn url_from_fields(config: &ConnectionConfig) -> String {
    let srv = config
        .options
        .iter()
        .any(|(k, v)| k == "srv" && matches!(v.as_str(), "true" | "1" | "yes"));
    let mut url = String::from(if srv { "mongodb+srv://" } else { "mongodb://" });
    let user = config.user.trim();
    if !user.is_empty() {
        url.push_str(&utf8_percent_encode(user, USERINFO).to_string());
        if let Some(password) = &config.password {
            url.push(':');
            url.push_str(&utf8_percent_encode(password, USERINFO).to_string());
        }
        url.push('@');
    }
    let host = match config.host.trim() {
        "" => "localhost",
        host => host,
    };
    url.push_str(host);
    // An SRV address names its hosts and ports in DNS and may not carry a port.
    if !srv && !host.contains(',') && !host.contains(':') {
        url.push_str(&format!(":{}", config.port.unwrap_or(27017)));
    }
    url.push('/');
    url.push_str(&utf8_percent_encode(config.database.trim(), USERINFO).to_string());
    let mut first = true;
    for (key, value) in &config.options {
        if key == "srv" || key.is_empty() {
            continue;
        }
        url.push(if first { '?' } else { '&' });
        first = false;
        url.push_str(&utf8_percent_encode(key, USERINFO).to_string());
        url.push('=');
        url.push_str(&utf8_percent_encode(value, USERINFO).to_string());
    }
    url
}

/// The driver's message without the topology dump it appends to selection errors.
fn brief(e: &mongodb::error::Error) -> String {
    let text = match e.kind.as_ref() {
        mongodb::error::ErrorKind::ServerSelection { message, .. } => message.clone(),
        mongodb::error::ErrorKind::Authentication { message, .. } => message.clone(),
        mongodb::error::ErrorKind::InvalidArgument { message, .. } => message.clone(),
        other => other.to_string(),
    };
    match text.split_once(", Topology:") {
        Some((head, _)) => head.to_string(),
        None => text,
    }
}

fn connect_error(e: mongodb::error::Error) -> Error {
    if let mongodb::error::ErrorKind::Command(c) = e.kind.as_ref() {
        return Error::Connect(c.message.clone());
    }
    Error::Connect(brief(&e))
}

/// The server's message for the person, without the driver's error labels.
fn mongo_error(e: mongodb::error::Error) -> Error {
    use mongodb::error::{ErrorKind, WriteFailure};
    match e.kind.as_ref() {
        // Interrupted by killOp, or past its time limit.
        ErrorKind::Command(c) if c.code == 11601 || c.code == 11602 => Error::Cancelled,
        ErrorKind::Command(c) if c.code_name.is_empty() => Error::Query(c.message.clone()),
        ErrorKind::Command(c) => Error::Query(format!("{} ({})", c.message, c.code_name)),
        ErrorKind::Write(WriteFailure::WriteError(w)) => Error::Query(w.message.clone()),
        ErrorKind::Write(WriteFailure::WriteConcernError(w)) => {
            Error::Query(format!("The write was not acknowledged: {}", w.message))
        }
        ErrorKind::InsertMany(failure) => {
            let first = failure
                .write_errors
                .as_ref()
                .and_then(|errors| errors.first())
                .map(|w| format!("Document {}: {}", w.index + 1, w.message));
            match (first, &failure.write_concern_error) {
                (Some(message), _) => Error::Query(message),
                (None, Some(w)) => {
                    Error::Query(format!("The write was not acknowledged: {}", w.message))
                }
                (None, None) => Error::Query(brief(&e)),
            }
        }
        ErrorKind::Authentication { message, .. } => Error::Connect(message.clone()),
        ErrorKind::ServerSelection { .. } => {
            Error::Other(format!("The server could not be reached: {}", brief(&e)))
        }
        ErrorKind::Io(io) => Error::Other(format!("Network error: {io}")),
        ErrorKind::Shutdown => Error::Closed,
        ErrorKind::InvalidArgument { message, .. } => Error::Query(message.clone()),
        _ => Error::Other(brief(&e)),
    }
}

struct Cancel {
    inner: Arc<Inner>,
}

impl CancelHandle for Cancel {
    fn cancel(&self) -> BoxFuture<'static, Result<()>> {
        let inner = self.inner.clone();
        runtime::spawn(&self.inner.rt, async move {
            inner.cancel().await;
            Ok(())
        })
    }
}

impl Connection for MongoConnection {
    fn kind(&self) -> DriverKind {
        DriverKind::Mongodb
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            schemas: false,
            transactions: false,
            edit_rows: true,
            cancel: true,
            language: "mongo",
        }
    }

    fn server_version(&self) -> String {
        self.inner.version.clone()
    }

    fn namespaces(&self) -> BoxFuture<'_, Result<Vec<Namespace>>> {
        self.spawn(|inner| async move { inner.namespaces().await })
    }

    fn objects<'a>(&'a self, namespace: &'a [String]) -> BoxFuture<'a, Result<Vec<ObjectEntry>>> {
        let namespace = namespace.to_vec();
        self.spawn(|inner| async move { inner.objects(&namespace).await })
    }

    fn describe<'a>(&'a self, object: &'a ObjectRef) -> BoxFuture<'a, Result<ObjectInfo>> {
        let object = object.clone();
        self.spawn(|inner| async move { inner.describe(&object).await })
    }

    fn browse<'a>(
        &'a self,
        object: &'a ObjectRef,
        request: &'a BrowseRequest,
    ) -> BoxFuture<'a, Result<BrowsePage>> {
        let object = object.clone();
        let request = request.clone();
        self.spawn(|inner| async move {
            let mut op = Op::start(&inner);
            let tag = Bson::String(op.tag.clone());
            op.run(inner.browse(&object, &request, tag)).await
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
            let mut op = Op::start(&inner);
            let tag = Bson::String(op.tag.clone());
            op.run(inner.count(&object, &request, exact, tag)).await
        })
    }

    fn execute<'a>(
        &'a self,
        statement: &'a str,
        options: &'a ExecuteOptions,
    ) -> BoxFuture<'a, Result<RowStream>> {
        let statement = statement.to_string();
        let options = options.clone();
        self.spawn(|inner| inner.execute(statement, options))
    }

    fn apply_edits<'a>(
        &'a self,
        object: &'a ObjectRef,
        edits: &'a [RowEdit],
    ) -> BoxFuture<'a, Result<u64>> {
        let object = object.clone();
        let edits = edits.to_vec();
        self.spawn(|inner| async move { inner.apply_edits(&object, &edits).await })
    }

    fn cancel_handle(&self) -> Option<Arc<dyn CancelHandle>> {
        Some(Arc::new(Cancel {
            inner: self.inner.clone(),
        }))
    }

    fn close(&self) -> BoxFuture<'_, ()> {
        let client = self.inner.client.clone();
        let task = self.inner.rt.spawn(async move {
            client.shutdown().immediate(true).await;
        });
        Box::pin(async move {
            let _ = task.await;
        })
    }
}

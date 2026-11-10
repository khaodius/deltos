//! Test doubles: a connection and a loader over tens of millions of virtual rows that are
//! computed on demand rather than stored, and a single-threaded executor that stands in
//! for the app's runtime so every test is deterministic.

use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::task::{Context, Poll};

use deltos_data::{
    BrowsePage, BrowseRequest, Capabilities, Column, Connection, DriverKind, Error, ExecuteOptions,
    Namespace, ObjectEntry, ObjectInfo, ObjectRef, Result, Row, RowBatch, RowStream, Value,
    ValueKind,
};
use futures::channel::oneshot;
use futures::future::BoxFuture;
use futures::task::{ArcWake, waker_ref};
use parking_lot::Mutex;

use crate::loader::{Hooks, LoadedPage, PageLoader, PageRequest, Reporter, RowCount};

/// The columns of the fake tables: `id`, `name`, `score`, `even`, then JSON extras.
pub fn fake_columns(count: usize) -> Vec<Column> {
    (0..count)
        .map(|c| match c {
            0 => Column::new("id", "int8", ValueKind::Int),
            1 => Column::new("name", "text", ValueKind::Text),
            2 => Column::new("score", "float8", ValueKind::Float),
            3 => Column::new("even", "bool", ValueKind::Bool),
            n => Column::new(format!("extra{n}"), "jsonb", ValueKind::Json),
        })
        .collect()
}

/// Row `index` of a fake table, the same every time.
pub fn fake_row(index: u64, columns: usize) -> Row {
    (0..columns)
        .map(|c| match c {
            0 => Value::Int(index as i64),
            1 => Value::text(format!("row-{index}")),
            2 => Value::Float(index as f64 * 0.25),
            3 => Value::Bool(index.is_multiple_of(2)),
            n => Value::Json(format!("{{\"n\":{n},\"i\":{index}}}").into()),
        })
        .collect()
}

struct Task {
    future: Mutex<Option<BoxFuture<'static, ()>>>,
    executor: Weak<ExecutorInner>,
    queued: AtomicBool,
}

impl ArcWake for Task {
    fn wake_by_ref(task: &Arc<Self>) {
        if !task.queued.swap(true, Ordering::AcqRel)
            && let Some(executor) = task.executor.upgrade()
        {
            executor.queue.lock().push_back(task.clone());
        }
    }
}

#[derive(Default)]
struct ExecutorInner {
    queue: Mutex<VecDeque<Arc<Task>>>,
    notifications: AtomicUsize,
}

/// Runs spawned futures on the test thread, in the order they became ready, until none
/// can make progress. Blocking work runs inline.
#[derive(Clone, Default)]
pub struct TestExecutor {
    inner: Arc<ExecutorInner>,
}

impl TestExecutor {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn hooks(&self) -> Hooks {
        let spawn_target = Arc::downgrade(&self.inner);
        let notify_target = self.inner.clone();
        Hooks {
            notify: Arc::new(move || {
                notify_target.notifications.fetch_add(1, Ordering::Relaxed);
            }),
            spawn: Arc::new(move |future| {
                let task = Arc::new(Task {
                    future: Mutex::new(Some(future)),
                    executor: spawn_target.clone(),
                    queued: AtomicBool::new(false),
                });
                Task::wake_by_ref(&task);
            }),
            spawn_blocking: Arc::new(|f| f()),
        }
    }

    /// Polls until every task is pending or finished.
    pub fn run(&self) {
        let mut polls = 0u64;
        loop {
            let next = self.inner.queue.lock().pop_front();
            let Some(task) = next else { break };
            task.queued.store(false, Ordering::Release);
            let mut slot = task.future.lock();
            let Some(future) = slot.as_mut() else {
                continue;
            };
            let waker = waker_ref(&task);
            let mut cx = Context::from_waker(&waker);
            if let Poll::Ready(()) = future.as_mut().poll(&mut cx) {
                *slot = None;
            }
            polls += 1;
            assert!(polls < 50_000_000, "the executor is spinning");
        }
    }

    pub fn notifications(&self) -> usize {
        self.inner.notifications.load(Ordering::Relaxed)
    }
}

type ColumnsFn = Arc<dyn Fn(u64) -> Vec<Column> + Send + Sync>;

#[derive(Default)]
struct FakeState {
    requests: Vec<PageRequest>,
    columns_for_page: Option<ColumnsFn>,
    hold: bool,
    gates: VecDeque<oneshot::Sender<()>>,
    count: Option<RowCount>,
    failing: HashSet<u64>,
    reporter: Option<Reporter>,
}

/// A [`PageLoader`] over a virtual table of `rows` rows, with switches for what the
/// cache tests need: holding loads open, failing pages and varying the column list.
pub struct FakeLoader {
    rows: u64,
    state: Mutex<FakeState>,
}

impl FakeLoader {
    pub fn new(rows: u64) -> Self {
        Self {
            rows,
            state: Mutex::new(FakeState::default()),
        }
    }

    pub fn requested_pages(&self) -> Vec<u64> {
        self.state.lock().requests.iter().map(|r| r.page).collect()
    }

    pub fn clear_requests(&self) {
        self.state.lock().requests.clear();
    }

    /// Makes every page report its own column list; rows get one value per column.
    pub fn set_columns_for_page(&self, f: impl Fn(u64) -> Vec<Column> + Send + Sync + 'static) {
        self.state.lock().columns_for_page = Some(Arc::new(f));
    }

    /// From now on loads wait until released.
    pub fn hold(&self) {
        self.state.lock().hold = true;
    }

    pub fn held(&self) -> usize {
        self.state.lock().gates.len()
    }

    pub fn release_oldest(&self, n: usize) {
        let mut state = self.state.lock();
        for _ in 0..n {
            if let Some(gate) = state.gates.pop_front() {
                let _ = gate.send(());
            }
        }
    }

    pub fn release_all(&self) {
        let n = self.held();
        self.release_oldest(n);
    }

    pub fn set_count(&self, count: RowCount) {
        self.state.lock().count = Some(count);
    }

    pub fn fail_page(&self, page: u64) {
        self.state.lock().failing.insert(page);
    }

    pub fn clear_failures(&self) {
        self.state.lock().failing.clear();
    }
}

impl PageLoader for FakeLoader {
    fn random_access(&self) -> bool {
        true
    }

    fn load(&self, request: PageRequest) -> BoxFuture<'static, Result<LoadedPage>> {
        let mut state = self.state.lock();
        state.requests.push(request);
        let columns: Arc<[Column]> = match &state.columns_for_page {
            Some(f) => f(request.page).into(),
            None => fake_columns(4).into(),
        };
        let result = if state.failing.contains(&request.page) {
            Err(Error::Query(format!("page {} failed", request.page)))
        } else {
            let end = (request.start + u64::from(request.rows)).min(self.rows);
            let rows = (request.start.min(end)..end)
                .map(|i| fake_row(i, columns.len()))
                .collect();
            Ok(LoadedPage {
                columns,
                rows,
                end: end >= self.rows,
            })
        };
        if state.hold {
            let (tx, rx) = oneshot::channel();
            state.gates.push_back(tx);
            Box::pin(async move {
                let _ = rx.await;
                result
            })
        } else {
            Box::pin(futures::future::ready(result))
        }
    }

    fn count(&self) -> BoxFuture<'static, Result<RowCount>> {
        let count = self
            .state
            .lock()
            .count
            .unwrap_or(RowCount::Exact(self.rows));
        Box::pin(futures::future::ready(Ok(count)))
    }

    fn attach(&self, reporter: Reporter) {
        self.state.lock().reporter = Some(reporter);
    }
}

/// Request counters for the seek-versus-offset tests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FakeStats {
    pub seek_requests: u64,
    pub offset_requests: u64,
    pub estimate_counts: u64,
    pub exact_counts: u64,
}

/// A [`Connection`] over a virtual table whose rows are [`fake_row`]. `browse` seeks when
/// asked to and counts how it was asked; `execute` streams the table in batches.
pub struct FakeConnection {
    rows: u64,
    columns: usize,
    estimate: Option<u64>,
    /// Make the stream fail after this many rows.
    stream_error_at: Option<u64>,
    stats: Mutex<FakeStats>,
}

impl FakeConnection {
    pub fn new(rows: u64) -> Self {
        Self {
            rows,
            columns: 4,
            estimate: None,
            stream_error_at: None,
            stats: Mutex::new(FakeStats::default()),
        }
    }

    pub fn with_estimate(mut self, estimate: u64) -> Self {
        self.estimate = Some(estimate);
        self
    }

    pub fn with_stream_error_at(mut self, rows: u64) -> Self {
        self.stream_error_at = Some(rows);
        self
    }

    pub fn stats(&self) -> FakeStats {
        *self.stats.lock()
    }

    pub fn object() -> ObjectRef {
        ObjectRef {
            namespace: vec!["main".into()],
            name: "rows".into(),
            kind: deltos_data::ObjectKind::Table,
        }
    }
}

impl Connection for FakeConnection {
    fn kind(&self) -> DriverKind {
        DriverKind::Sqlite
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::default()
    }

    fn server_version(&self) -> String {
        "fake".into()
    }

    fn namespaces(&self) -> BoxFuture<'_, Result<Vec<Namespace>>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    fn objects<'a>(&'a self, _namespace: &'a [String]) -> BoxFuture<'a, Result<Vec<ObjectEntry>>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    fn describe<'a>(&'a self, _object: &'a ObjectRef) -> BoxFuture<'a, Result<ObjectInfo>> {
        Box::pin(async move {
            Ok(ObjectInfo {
                columns: fake_columns(self.columns),
                ..ObjectInfo::default()
            })
        })
    }

    fn browse<'a>(
        &'a self,
        _object: &'a ObjectRef,
        request: &'a BrowseRequest,
    ) -> BoxFuture<'a, Result<BrowsePage>> {
        Box::pin(async move {
            let seek = request.seek_after.as_ref().and_then(|key| {
                key.iter().find_map(|(name, value)| match value {
                    Value::Int(id) if name == "id" => Some(*id as u64 + 1),
                    _ => None,
                })
            });
            let start = {
                let mut stats = self.stats.lock();
                match seek {
                    Some(start) => {
                        stats.seek_requests += 1;
                        start
                    }
                    None => {
                        stats.offset_requests += 1;
                        request.offset
                    }
                }
            };
            let end = (start + u64::from(request.limit)).min(self.rows);
            let rows: Vec<Row> = (start.min(end)..end)
                .map(|i| fake_row(i, self.columns))
                .collect();
            let last_key = rows
                .last()
                .map(|row| vec![("id".to_string(), row[0].clone())]);
            Ok(BrowsePage {
                columns: fake_columns(self.columns).into(),
                rows,
                more: end < self.rows,
                last_key,
            })
        })
    }

    fn count<'a>(
        &'a self,
        _object: &'a ObjectRef,
        _request: &'a BrowseRequest,
        exact: bool,
    ) -> BoxFuture<'a, Result<u64>> {
        Box::pin(async move {
            let mut stats = self.stats.lock();
            if exact {
                stats.exact_counts += 1;
                Ok(self.rows)
            } else {
                stats.estimate_counts += 1;
                Ok(self.estimate.unwrap_or(self.rows))
            }
        })
    }

    fn execute<'a>(
        &'a self,
        _statement: &'a str,
        options: &'a ExecuteOptions,
    ) -> BoxFuture<'a, Result<RowStream>> {
        let total = self.rows;
        let columns = self.columns;
        let batch = u64::from(options.batch_size.max(1));
        let error_at = self.stream_error_at;
        Box::pin(async move {
            let batches = futures::stream::unfold(0u64, move |start| async move {
                if start >= total {
                    return None;
                }
                if error_at.is_some_and(|at| start >= at) {
                    return Some((Err(Error::Query("the stream broke".into())), total));
                }
                let end = (start + batch).min(total);
                let rows = (start..end).map(|i| fake_row(i, columns)).collect();
                Some((Ok(RowBatch { rows }), end))
            });
            Ok(RowStream {
                columns: fake_columns(columns).into(),
                batches: Box::pin(batches),
                affected: None,
                messages: Vec::new(),
            })
        })
    }
}

//! Pages for a statement's result. A [`RowStream`] can only be read forwards, so its
//! batches are pulled as they are needed and appended to a [`SpillStore`]; pages are then
//! decoded from disk into the cache like any other.
//!
//! Reading is driven by demand. A request for a page that is not on disk yet pulls the
//! stream until it is, so with the cache's prefetch the stream stays a couple of pages
//! ahead of the viewport and no further: scrolling to the bottom of what is known reads
//! on, standing still reads nothing. [`StreamLoader::fetch_all`] pulls to the end in
//! the background for a person who wants the whole result (and a final row count), one
//! batch per turn so page loads interleave with it. A `max_spill_bytes` cap stops
//! reading with a message that says so; what was read stays browsable.
//!
//! Disk work goes through the blocking hook of the [`Reporter`] the cache attached. Used
//! standalone (the CSV export does this) it runs inline on the calling task.

use std::path::PathBuf;
use std::sync::Arc;

use deltos_data::{Column, Error, Result, RowBatch, RowStream};
use futures::StreamExt;
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use parking_lot::Mutex;

use crate::loader::{
    Hooks, LoadedPage, LoaderEvent, PageLoader, PageRequest, Reporter, RowCount, run_blocking,
};
use crate::spill::{self, SpillStore};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamConfig {
    /// Rows per page; the same as the cache's `page_rows`.
    pub page_rows: u32,
    /// Stop reading once the spill file is this large. `None` reads to the end.
    pub max_spill_bytes: Option<u64>,
    /// Where to put the spill file; the system temporary directory by default.
    pub temp_dir: Option<PathBuf>,
}

impl Default for StreamConfig {
    fn default() -> Self {
        Self {
            page_rows: 500,
            max_spill_bytes: None,
            temp_dir: None,
        }
    }
}

#[derive(Default)]
struct State {
    /// The stream produced its last batch.
    ended: bool,
    /// Reading stopped early, and why.
    stopped: Option<String>,
    fetch_all: bool,
    fetching: bool,
    reporter: Option<Reporter>,
}

enum Pulled {
    Rows,
    Ended,
    Stopped(String),
}

struct Inner {
    columns: Arc<[Column]>,
    affected: Option<u64>,
    messages: Vec<String>,
    config: StreamConfig,
    /// The stream, under an async lock so one puller owns it at a time and others wait
    /// without blocking a thread. `None` once it ended or was dropped.
    source: futures::lock::Mutex<Option<BoxStream<'static, Result<RowBatch>>>>,
    store: Arc<Mutex<SpillStore>>,
    state: Mutex<State>,
}

/// A [`PageLoader`] over one statement's result.
pub struct StreamLoader {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for StreamLoader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamLoader")
            .field("rows", &self.rows_read())
            .field("complete", &self.is_complete())
            .finish_non_exhaustive()
    }
}

impl StreamLoader {
    /// Takes the result of [`deltos_data::Connection::execute`]. Creating the spill file
    /// is the one piece of disk work done on the caller's thread.
    pub fn new(stream: RowStream, config: StreamConfig) -> std::io::Result<Self> {
        let store = SpillStore::create(config.page_rows, config.temp_dir.as_deref())?;
        Ok(Self {
            inner: Arc::new(Inner {
                columns: stream.columns,
                affected: stream.affected,
                messages: stream.messages,
                config,
                source: futures::lock::Mutex::new(Some(stream.batches)),
                store: Arc::new(Mutex::new(store)),
                state: Mutex::new(State::default()),
            }),
        })
    }

    pub fn columns(&self) -> Arc<[Column]> {
        self.inner.columns.clone()
    }

    /// Rows per page on disk. Pages are requested at this size.
    pub fn page_rows(&self) -> u32 {
        self.inner.config.page_rows
    }

    pub fn affected(&self) -> Option<u64> {
        self.inner.affected
    }

    pub fn messages(&self) -> &[String] {
        &self.inner.messages
    }

    /// Rows read from the stream so far.
    pub fn rows_read(&self) -> u64 {
        self.inner.store.lock().rows()
    }

    pub fn spill_bytes(&self) -> u64 {
        self.inner.store.lock().bytes()
    }

    /// Whether the stream was read to its end.
    pub fn is_complete(&self) -> bool {
        self.inner.state.lock().ended
    }

    /// Why reading stopped early, if it did: the spill cap, a failed write or a stream
    /// error.
    pub fn stopped_reason(&self) -> Option<String> {
        self.inner.state.lock().stopped.clone()
    }

    /// Keeps pulling to the end in the background. Returns false when no cache has
    /// attached yet, since the work needs the runtime hooks that come with it.
    pub fn fetch_all(&self) -> bool {
        let reporter = {
            let mut state = self.inner.state.lock();
            state.fetch_all = true;
            if state.fetching || state.ended || state.stopped.is_some() {
                return true;
            }
            let Some(reporter) = state.reporter.clone() else {
                return false;
            };
            state.fetching = true;
            reporter
        };
        reporter.report(LoaderEvent::Reading(true));
        let inner = self.inner.clone();
        let events = reporter.clone();
        reporter.spawn(Box::pin(async move {
            while inner.state.lock().fetch_all {
                if !matches!(inner.pull_batch().await, Pulled::Rows) {
                    break;
                }
            }
            inner.state.lock().fetching = false;
            events.report(LoaderEvent::Reading(false));
        }));
        true
    }

    /// Lets a running `fetch_all` stop after its current batch.
    pub fn stop_fetching(&self) {
        self.inner.state.lock().fetch_all = false;
    }
}

impl Inner {
    fn hooks(&self) -> Option<Hooks> {
        self.state
            .lock()
            .reporter
            .as_ref()
            .map(|r| r.hooks().clone())
    }

    fn report(&self, event: LoaderEvent) {
        let reporter = self.state.lock().reporter.clone();
        if let Some(reporter) = reporter {
            reporter.report(event);
        }
    }

    fn stop(&self, message: String) -> Pulled {
        self.state.lock().stopped = Some(message.clone());
        self.report(LoaderEvent::Error(message.clone()));
        Pulled::Stopped(message)
    }

    /// Pulls one batch onto disk. Holds the stream for the duration so pulls never
    /// overlap; reports the new total.
    async fn pull_batch(self: &Arc<Self>) -> Pulled {
        let mut source = self.source.lock().await;
        let Some(stream) = source.as_mut() else {
            let state = self.state.lock();
            return match &state.stopped {
                Some(message) => Pulled::Stopped(message.clone()),
                None => Pulled::Ended,
            };
        };
        match stream.next().await {
            None => {
                *source = None;
                let rows = self.store.lock().rows();
                self.state.lock().ended = true;
                self.report(LoaderEvent::RowCount(RowCount::Exact(rows)));
                self.report(LoaderEvent::Finished);
                Pulled::Ended
            }
            Some(Err(e)) => {
                *source = None;
                self.stop(e.to_string())
            }
            Some(Ok(batch)) => {
                if batch.rows.is_empty() {
                    return Pulled::Rows;
                }
                let store = self.store.clone();
                let appended = run_blocking(self.hooks().as_ref(), move || {
                    let mut store = store.lock();
                    store
                        .append(&batch.rows)
                        .map(|()| (store.rows(), store.bytes()))
                })
                .await;
                let (rows, bytes) = match appended {
                    Ok(Ok(totals)) => totals,
                    Ok(Err(e)) => {
                        *source = None;
                        return self.stop(format!("could not write the spill file: {e}"));
                    }
                    Err(e) => {
                        *source = None;
                        return self.stop(e.to_string());
                    }
                };
                if let Some(max) = self.config.max_spill_bytes
                    && bytes > max
                {
                    *source = None;
                    self.report(LoaderEvent::RowCount(RowCount::AtLeast(rows)));
                    return self.stop(format!(
                        "Stopped reading after {rows} rows: the result passed the {} spill limit. \
                         Narrow the query or raise the limit to see more.",
                        human_bytes(max)
                    ));
                }
                self.report(LoaderEvent::RowCount(RowCount::AtLeast(rows)));
                Pulled::Rows
            }
        }
    }

    /// Pulls until `target` rows are on disk or the stream is over.
    async fn ensure_rows(self: &Arc<Self>, target: u64) -> Result<()> {
        loop {
            if self.store.lock().rows() >= target {
                return Ok(());
            }
            {
                let state = self.state.lock();
                if state.ended || state.stopped.is_some() {
                    return Ok(());
                }
            }
            match self.pull_batch().await {
                Pulled::Rows => {}
                Pulled::Ended => return Ok(()),
                Pulled::Stopped(message) => return Err(Error::Other(message)),
            }
        }
    }

    async fn decode(self: &Arc<Self>, request: PageRequest) -> Result<LoadedPage> {
        let (file, span) = {
            let store = self.store.lock();
            (store.file(), store.page_span(request.page))
        };
        let rows = match span {
            Some(span) => {
                run_blocking(self.hooks().as_ref(), move || {
                    spill::read_span(&file, &span)
                })
                .await??
            }
            None => Vec::new(),
        };
        let ended = self.state.lock().ended;
        let total = self.store.lock().rows();
        Ok(LoadedPage {
            columns: self.columns.clone(),
            rows,
            end: ended && request.start.saturating_add(u64::from(request.rows)) >= total,
        })
    }
}

impl PageLoader for StreamLoader {
    fn random_access(&self) -> bool {
        false
    }

    fn load(&self, request: PageRequest) -> BoxFuture<'static, Result<LoadedPage>> {
        let inner = self.inner.clone();
        Box::pin(async move {
            let target = request.start.saturating_add(u64::from(request.rows));
            if inner.store.lock().rows() < target {
                let stopped = inner.state.lock().stopped.is_some();
                if !stopped {
                    inner.ensure_rows(target).await?;
                }
            }
            inner.decode(request).await
        })
    }

    fn count(&self) -> BoxFuture<'static, Result<RowCount>> {
        let rows = self.rows_read();
        let count = if self.is_complete() {
            RowCount::Exact(rows)
        } else {
            RowCount::AtLeast(rows)
        };
        Box::pin(futures::future::ready(Ok(count)))
    }

    fn attach(&self, reporter: Reporter) {
        reporter.report(LoaderEvent::Columns(self.inner.columns.clone()));
        self.inner.state.lock().reporter = Some(reporter);
    }
}

fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["bytes", "KiB", "MiB", "GiB", "TiB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} bytes")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::{CacheConfig, GridCache};
    use crate::loader::Status;
    use crate::testing::{FakeConnection, TestExecutor, fake_row};
    use deltos_data::{Connection, ExecuteOptions, Value};

    fn open(connection: &FakeConnection, config: StreamConfig) -> Arc<StreamLoader> {
        let options = ExecuteOptions {
            batch_size: 1000,
            ..ExecuteOptions::default()
        };
        let stream = futures::executor::block_on(connection.execute("select", &options)).unwrap();
        Arc::new(StreamLoader::new(stream, config).unwrap())
    }

    fn cache_config() -> CacheConfig {
        CacheConfig {
            page_rows: 500,
            max_bytes: 64 << 20,
            prefetch_pages: 2,
            max_concurrent_loads: 2,
        }
    }

    #[test]
    fn reads_as_the_viewport_advances_then_everything_on_request() {
        let exec = TestExecutor::new();
        let connection = FakeConnection::new(100_000);
        let loader = open(&connection, StreamConfig::default());
        let cache = GridCache::new(loader.clone(), cache_config(), exec.hooks());
        exec.run();
        assert_eq!(
            cache.columns().map(|c| c.len()),
            Some(4),
            "columns before any row"
        );
        assert_eq!(cache.row_count(), RowCount::AtLeast(0));

        cache.set_viewport(0..40);
        exec.run();
        // Page 0 and two prefetched pages need 1500 rows: two batches of 1000.
        assert_eq!(loader.rows_read(), 2000);
        assert_eq!(cache.row_count(), RowCount::AtLeast(2000));
        assert_eq!(cache.status(), Status::Idle);
        assert_eq!(cache.with_row(39, |r| r[0].clone()), Some(Value::Int(39)));

        cache.set_viewport(1900..1940);
        exec.run();
        assert_eq!(loader.rows_read(), 3000, "reading follows the viewport");
        assert!(cache.is_loaded(1939));

        assert!(loader.fetch_all());
        assert_eq!(cache.status(), Status::Loading);
        exec.run();
        assert_eq!(loader.rows_read(), 100_000);
        assert!(loader.is_complete());
        assert_eq!(cache.row_count(), RowCount::Exact(100_000));
        assert_eq!(cache.status(), Status::Done);
        assert!(loader.spill_bytes() > 0);

        cache.set_viewport(99_990..100_000);
        exec.run();
        assert_eq!(
            cache.with_row(99_999, |r| r.to_vec()),
            Some(fake_row(99_999, 4).to_vec())
        );
        assert!(cache.with_row(100_000, |_| ()).is_none());
    }

    #[test]
    fn spill_cap_stops_reading_with_a_clear_status() {
        let exec = TestExecutor::new();
        let connection = FakeConnection::new(1_000_000);
        let loader = open(
            &connection,
            StreamConfig {
                max_spill_bytes: Some(200_000),
                ..StreamConfig::default()
            },
        );
        let cache = GridCache::new(loader.clone(), cache_config(), exec.hooks());
        loader.fetch_all();
        exec.run();
        let read = loader.rows_read();
        assert!((1000..20_000).contains(&read), "{read} rows");
        assert!(!loader.is_complete());
        let reason = loader.stopped_reason().unwrap();
        assert!(reason.contains("spill limit"), "{reason}");
        assert_eq!(cache.status(), Status::Error(reason));
        assert_eq!(cache.row_count(), RowCount::AtLeast(read));

        // What was read is still there; what was not is empty rather than an error.
        cache.set_viewport(0..10);
        exec.run();
        assert!(cache.is_loaded(9));
        cache.set_viewport(read + 5000..read + 5010);
        exec.run();
        assert!(!cache.is_loaded(read + 5000));
        assert_eq!(connection.stats().exact_counts, 0);
    }

    #[test]
    fn stream_error_is_reported_and_earlier_rows_stay() {
        let exec = TestExecutor::new();
        let connection = FakeConnection::new(10_000).with_stream_error_at(3000);
        let loader = open(&connection, StreamConfig::default());
        let cache = GridCache::new(loader.clone(), cache_config(), exec.hooks());
        cache.set_viewport(2800..2810);
        exec.run();
        assert_eq!(loader.rows_read(), 3000);
        assert!(cache.is_loaded(2809));
        assert_eq!(cache.status(), Status::Error("the stream broke".into()));
        assert_eq!(loader.stopped_reason().as_deref(), Some("the stream broke"));
    }

    #[test]
    fn last_page_is_marked_as_the_end_and_file_goes_away() {
        let connection = FakeConnection::new(1234);
        let loader = open(&connection, StreamConfig::default());
        assert!(
            !loader.fetch_all(),
            "no cache attached: nothing to spawn on"
        );
        let path = loader.inner.store.lock().path().to_path_buf();
        let page = futures::executor::block_on(loader.load(PageRequest::new(2, 500))).unwrap();
        assert_eq!(page.rows.len(), 234);
        assert!(page.end);
        let beyond = futures::executor::block_on(loader.load(PageRequest::new(7, 500))).unwrap();
        assert!(beyond.rows.is_empty() && beyond.end);
        assert_eq!(
            futures::executor::block_on(loader.count()).unwrap(),
            RowCount::Exact(1234)
        );
        drop(loader);
        assert!(!path.exists());
    }

    #[test]
    fn human_sizes() {
        assert_eq!(human_bytes(512), "512 bytes");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(human_bytes(1 << 30), "1.0 GiB");
    }
}
